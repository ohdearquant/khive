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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use uuid::Uuid;

use crate::source::{cache_key, redact_repo_url};

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

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn max_repos() -> usize {
    std::env::var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_REPOS)
}

fn max_total_bytes() -> u64 {
    env_u64("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", DEFAULT_MAX_TOTAL_BYTES)
}

fn clone_max_bytes() -> u64 {
    env_u64("KHIVE_GIT_DIGEST_CLONE_MAX_BYTES", DEFAULT_CLONE_MAX_BYTES)
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
    clone(canonical_url, &staging_dir).inspect_err(|_| {
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
    let target_fd = unix_fd::openat_dir_nofollow(&root_fd, name)
        .map_err(|_| CacheError::UnsafeToReplace(repo_dir.to_path_buf()))?;
    if !is_owned_entry_via_fd(&target_fd) {
        return Err(CacheError::UnsafeToReplace(repo_dir.to_path_buf()));
    }
    let target_id = unix_fd::fstat(&target_fd)
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
    let moved_id = unix_fd::fstatat_nofollow(&namespace_fd, trash_name_os).map_err(|e| {
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
/// Non-unix carries no descriptor and keeps the pathname `--git-dir` form
/// (weaker; documented at `revalidate_owned_slot`).
struct ValidatedSlot {
    #[cfg(unix)]
    git_dir: std::fs::File,
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
        let git_dir =
            unix_fd::openat_dir_nofollow(&parent, std::ffi::OsStr::new(".git")).unwrap_or(parent);
        Self { git_dir }
    }
}

/// Start a `git` invocation addressed at the validated slot. Unix: the child
/// `fchdir`s into the validated `.git` descriptor before exec and uses
/// `--git-dir .`, so git operates on the descriptor-resolved `.git` object and
/// never re-resolves the name `.git` (which a relative `--git-dir .git` would,
/// following a `.git` symlink swapped in after validation). Non-unix: absolute
/// `--git-dir` on the pathname. All git commands here are ref/git-dir-only
/// (fetch, remote set-head, symbolic-ref, update-ref); none needs a work tree,
/// so cwd being the git dir is correct.
/// Callers that only read the exit status must null stdout themselves — git
/// ref chatter ("origin/HEAD is unchanged...") otherwise interleaves with
/// the process's own stdout protocol stream. (Not nulled here: `.output()`
/// callers need stdout captured, and an explicit null would empty it.)
fn git_at_slot(repo: &Path, slot: &ValidatedSlot) -> Command {
    let mut cmd = Command::new("git");
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
    #[cfg(not(unix))]
    {
        // Pathname-bound fallback: non-Unix targets cannot pass a directory
        // descriptor to git, so `.git` is re-resolved by name here. This
        // reopens the symlink-swap TOCTOU the Unix descriptor-pin closes — a
        // `.git` swapped for a symlink after validation redirects git to an
        // unowned repository. Windows-safe handle-pinning needs platform APIs
        // untestable on this CI and is tracked in #2149.
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
    // `openat_dir_nofollow` refuses that symlink (`ELOOP`) and otherwise pins
    // the exact `.git` inode, so `git_at_slot`'s `--git-dir .` can never reach
    // outside the validated slot.
    let git_dir = unix_fd::openat_dir_nofollow(&fd, std::ffi::OsStr::new(".git"))
        .map_err(|_| CacheError::UnsafeToReplace(repo_dir.to_path_buf()))?;
    Ok(ValidatedSlot { git_dir })
}

/// Non-unix fallback: pathname re-check at the same call site. Weaker than
/// the fd-bound form — the explicit `--git-dir` layer still prevents upward
/// discovery, though not a symlink swapped in after this check. That
/// pathname-bound TOCTOU is tracked in #2149; the Windows-safe handle-pin
/// needs platform APIs untestable on this CI.
#[cfg(not(unix))]
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
    let git_is_directory = unix_fd::fstatat_nofollow(target_fd, std::ffi::OsStr::new(".git"))
        .is_ok_and(|st| (st.st_mode & libc::S_IFMT) == libc::S_IFDIR);
    let marker_is_regular_file =
        unix_fd::fstatat_nofollow(target_fd, std::ffi::OsStr::new(MARKER_FILE))
            .is_ok_and(|st| (st.st_mode & libc::S_IFMT) == libc::S_IFREG);
    git_is_directory && marker_is_regular_file
}

/// `openat`/`fstatat`/`renameat` primitives bound to an already-opened
/// directory descriptor rather than a pathname, mirroring the
/// `O_NOFOLLOW`/`fstat` idiom used in `khive-db`'s WAL-pin sidecar and
/// `khive-vamana`'s external-id sidecar: every operation after the initial
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

    fn cstring(component: &OsStr) -> io::Result<CString> {
        CString::new(component.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "path component contains a NUL byte",
            )
        })
    }

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

    /// `openat(dir, name, O_DIRECTORY | O_NOFOLLOW)` — open the single path
    /// component `name` as a directory relative to `dir`'s own descriptor,
    /// refusing a symlink at that component.
    pub(super) fn openat_dir_nofollow(dir: &fs::File, name: &OsStr) -> io::Result<fs::File> {
        let c_name = cstring(name)?;
        // SAFETY: `dir.as_raw_fd()` is a live directory descriptor;
        // `c_name` is NUL-terminated for the duration of the call.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by a successful `openat`.
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }

    /// `fstatat(dir, name, AT_SYMLINK_NOFOLLOW)` — the identity of `name`
    /// as it stands right now, resolved relative to `dir`'s own descriptor
    /// rather than a fresh pathname walk from the filesystem root.
    pub(super) fn fstatat_nofollow(dir: &fs::File, name: &OsStr) -> io::Result<libc::stat> {
        let c_name = cstring(name)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `dir.as_raw_fd()` is live; `c_name` is NUL-terminated;
        // `st` is a valid out-param for the duration of the call.
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                c_name.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }

    pub(super) fn fstat(file: &fs::File) -> io::Result<libc::stat> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `file.as_raw_fd()` is live; `st` is a valid out-param for
        // the duration of the call.
        let rc = unsafe { libc::fstat(file.as_raw_fd(), &mut st) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
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
        let c_name = cstring(name)?;
        let c_to_name = cstring(to_name)?;
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
fn clone(url: &str, dest: &Path) -> Result<(), CacheError> {
    let status = Command::new("git")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("-c")
        .arg("maintenance.auto=false")
        .arg("clone")
        .arg("--filter=blob:none")
        .arg("--no-checkout")
        .arg(url)
        .arg(dest)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::null())
        .status()
        .map_err(|e| CacheError::Git(format!("spawning git clone: {e}")))?;
    if !status.success() {
        return Err(CacheError::Git(format!(
            "git clone {:?} failed (exit {status})",
            redact_repo_url(url)
        )));
    }
    Ok(())
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
    let _ = git_at_slot(repo, slot)
        .args(["remote", "set-head", "origin", "--auto"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::null())
        .status();

    // A fresh slot's HEAD is a symref to the default branch, so the branch is
    // what has to move for `rev-parse HEAD` and `log` to resolve at the tip.
    // A slot whose HEAD is already detached has no branch to move, and there
    // the HEAD file itself is the target. Both shapes occur: the first is
    // what `git clone` produces, the second is reachable through repair paths
    // and through slots older than this code.
    let symref = git_at_slot(repo, slot)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
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
    let status = cmd
        .arg("refs/remotes/origin/HEAD")
        .stdout(Stdio::null())
        .status()
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
    let status = git_at_slot(repo, slot)
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("-c")
        .arg("maintenance.auto=false")
        .arg("fetch")
        .arg("--prune")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::null())
        .status()
        .map_err(|e| CacheError::Git(format!("spawning git fetch: {e}")))?;
    if !status.success() {
        return Err(CacheError::Git(format!(
            "git fetch in {} failed (exit {status})",
            repo.display()
        )));
    }
    Ok(())
}

/// Issue #765 repair primitive: `git fetch --refetch origin` obtains a
/// complete fresh filtered packfile instead of incrementally trusting the
/// existing object store.
fn fetch_refetch(repo: &Path, slot: &ValidatedSlot) -> Result<(), CacheError> {
    // Descriptor-bound — see `fetch` for the discovery and symlink hazards.
    let status = git_at_slot(repo, slot)
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("-c")
        .arg("maintenance.auto=false")
        .arg("fetch")
        .arg("--refetch")
        .arg("origin")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::null())
        .status()
        .map_err(|e| CacheError::Git(format!("spawning git fetch --refetch: {e}")))?;
    if !status.success() {
        return Err(CacheError::Git(format!(
            "git fetch --refetch in {} failed (exit {status})",
            repo.display()
        )));
    }
    Ok(())
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
            let modified = std::fs::symlink_metadata(wrapper)
                .and_then(|m| m.modified())
                .map_err(|e| io_err("reap_stale_staging: wrapper mtime", wrapper, e))?;
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

        match staging_liveness(&path, now, max_age)? {
            StagingLiveness::Live => continue,
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
/// *descendant* disappearing mid-walk (contributes 0 bytes); the walk
/// **root** itself vanishing is NOT tolerated and surfaces as
/// `CacheError::Io(NotFound)`. See
/// crates/khive-pack-git/docs/api/cache.md#dir_size.
fn dir_size(path: &Path) -> Result<u64, CacheError> {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let is_root = p == path;
        let md = match std::fs::symlink_metadata(&p) {
            Ok(md) => md,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !is_root => continue,
            Err(e) => return Err(io_err("dir_size: stat", &p, e)),
        };
        if md.is_dir() {
            let read_dir = match std::fs::read_dir(&p) {
                Ok(read_dir) => read_dir,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !is_root => continue,
                Err(e) => return Err(io_err("dir_size: read_dir", &p, e)),
            };
            for entry in read_dir {
                match entry {
                    Ok(entry) => stack.push(entry.path()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(io_err("dir_size: read_dir entry", &p, e)),
                }
            }
        } else {
            total += md.len();
        }
    }
    Ok(total)
}

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
mod tests {
    use super::*;

    /// Build a directory shaped exactly like a real `ensure_clone` cache slot.
    fn make_owned_entry(root: &Path, key: &str, with_marker: bool) -> PathBuf {
        assert_eq!(key.len(), 16, "test cache keys must be 16 hex chars");
        let p = root.join(key);
        std::fs::create_dir_all(p.join(".git")).unwrap();
        if with_marker {
            std::fs::write(p.join(MARKER_FILE), b"").unwrap();
        }
        p
    }

    /// Build a staging wrapper directly (bypassing `install_fresh_clone`)
    /// under the private namespace, optionally with a lock file held open
    /// by the returned guard (drop the guard to simulate the owning
    /// process dying / releasing the lock).
    fn make_staging_wrapper(root: &Path, held: bool) -> (PathBuf, Uuid, Option<std::fs::File>) {
        let namespace_root = ensure_staging_namespace(root).expect("staging namespace");
        let id = Uuid::new_v4();
        let wrapper = namespace_root.join(id.to_string());
        std::fs::create_dir_all(wrapper.join("repo")).expect("create staging wrapper");
        let held_file = if held {
            let lock_path = wrapper.join(STAGING_LOCK_FILE);
            let f = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)
                .expect("open lock file");
            f.try_lock().expect("acquire lock");
            Some(f)
        } else {
            None
        };
        (wrapper, id, held_file)
    }

    fn slot_lock_registry_len() -> usize {
        SLOT_LOCKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    fn slot_lock_registry_capacity() -> usize {
        SLOT_LOCKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .capacity()
    }

    #[test]
    fn stale_staging_sweep_removes_an_abandoned_wrapper_lacking_a_lock_file_once_old() {
        let root = tempfile::tempdir().expect("tempdir");
        let (wrapper, _id, _held) = make_staging_wrapper(root.path(), false);
        std::fs::create_dir_all(wrapper.join("repo/partial.git/objects")).expect("nested payload");
        std::fs::write(wrapper.join("repo/partial.git/objects/pack"), b"partial")
            .expect("write orphan payload");
        let observed_mtime = std::fs::symlink_metadata(&wrapper)
            .expect("wrapper metadata")
            .modified()
            .expect("wrapper mtime");

        let removed = reap_stale_staging(
            root.path(),
            observed_mtime + std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(1),
        )
        .expect("reap stale staging directory");

        assert_eq!(removed, 1);
        assert!(
            !wrapper.exists(),
            "abandoned staging payload must be reclaimed"
        );
    }

    /// Blocking-finding acceptance test: a wrapper whose lock is still held
    /// by a live handle must survive the sweep no matter how far past the
    /// age fence it is -- age alone must never be the deletion criterion.
    #[test]
    fn stale_staging_sweep_preserves_a_wrapper_whose_lock_is_still_held_past_the_age_fence() {
        let root = tempfile::tempdir().expect("tempdir");
        let (wrapper, _id, held) = make_staging_wrapper(root.path(), true);
        let _held = held.expect("lock guard");

        let far_future = SystemTime::now() + std::time::Duration::from_secs(365 * 24 * 60 * 60);
        let removed =
            reap_stale_staging(root.path(), far_future, std::time::Duration::from_secs(1))
                .expect("sweep around a live wrapper");

        assert_eq!(removed, 0);
        assert!(
            wrapper.exists(),
            "a wrapper whose lock is still held by a live process must survive, \
             even a full year past the age fence"
        );
    }

    /// The flip side of the test above: liveness, not freshness, is what
    /// matters. An abandoned wrapper (its lock file exists but nothing
    /// holds the lock) is reclaimed even when it was created moments ago.
    #[test]
    fn stale_staging_sweep_removes_an_abandoned_wrapper_even_when_fresh() {
        let root = tempfile::tempdir().expect("tempdir");
        let (wrapper, _id, held) = make_staging_wrapper(root.path(), true);
        // Simulate the owning process dying: release the lock (dropping the
        // handle is exactly what the kernel does on process exit/kill).
        drop(held);

        let removed = reap_stale_staging(
            root.path(),
            SystemTime::now(),
            std::time::Duration::from_secs(24 * 60 * 60),
        )
        .expect("sweep an abandoned-but-fresh wrapper");

        assert_eq!(removed, 1);
        assert!(
            !wrapper.exists(),
            "an abandoned wrapper must be reclaimed even when it is not old"
        );
    }

    #[test]
    fn staging_sweep_preserves_foreign_nested_and_nondirectory_entries_even_when_stale() {
        let root = tempfile::tempdir().expect("tempdir");
        let namespace_root = ensure_staging_namespace(root.path()).expect("namespace");

        let live_id = Uuid::new_v4();
        let live_wrapper = namespace_root.join(live_id.to_string());
        std::fs::create_dir_all(&live_wrapper).expect("create live wrapper");

        let foreign = namespace_root.join("not-a-canonical-uuid");
        std::fs::create_dir_all(&foreign).expect("create foreign-named dir");
        let staging_file = namespace_root.join(Uuid::new_v4().to_string());
        std::fs::write(&staging_file, b"operator file").expect("write uuid-shaped file");
        let nested = namespace_root
            .join("operator-owned")
            .join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&nested).expect("create nested uuid-shaped dir");

        // Every entry above is missing its lock file, so the fallback age
        // check applies -- drive `now` far enough past `max_age` that every
        // candidate would be reclaimed by age alone. Only the containment,
        // name-shape, and type checks may save them (regression coverage
        // for the bug where a future-dated fixture never reached those
        // checks at all).
        let far_future = SystemTime::now() + std::time::Duration::from_secs(365 * 24 * 60 * 60);
        let removed =
            reap_stale_staging(root.path(), far_future, std::time::Duration::from_secs(1))
                .expect("scan namespace entries");

        assert_eq!(
            removed, 1,
            "only the canonical-UUID live wrapper is reclaimed"
        );
        assert!(!live_wrapper.exists());
        assert!(foreign.is_dir(), "a non-UUID name is not staging-shaped");
        assert!(staging_file.is_file(), "the sweep removes directories only");
        assert!(
            nested.is_dir(),
            "the sweep never descends below the namespace root"
        );
    }

    /// An interrupted `delete_verified_owned_entry` leaves its renamed
    /// `trash-<uuid>` slot behind with no lock file. The sweep must reclaim
    /// it once past the age fence (or the deletion path reintroduces the
    /// unreclaimable-residue class this module closes), must preserve it
    /// while fresh (an in-flight recursive delete), and must never touch a
    /// trash-prefixed name whose suffix is not a canonical UUID.
    #[test]
    fn staging_sweep_reclaims_old_trash_residue_but_preserves_fresh_and_lookalikes() {
        let root = tempfile::tempdir().expect("tempdir");
        let namespace_root = ensure_staging_namespace(root.path()).expect("namespace");

        let old_trash = namespace_root.join(format!("trash-{}", Uuid::new_v4()));
        std::fs::create_dir_all(old_trash.join("repo/.git/objects")).expect("old trash payload");
        let lookalike = namespace_root.join("trash-not-a-canonical-uuid");
        std::fs::create_dir_all(&lookalike).expect("create trash lookalike");
        let observed_mtime = std::fs::symlink_metadata(&old_trash)
            .expect("trash metadata")
            .modified()
            .expect("trash mtime");

        let removed = reap_stale_staging(
            root.path(),
            observed_mtime + std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(1),
        )
        .expect("reap trash residue");
        assert_eq!(removed, 1, "only the canonical trash residue is reclaimed");
        assert!(!old_trash.exists(), "aged trash residue must be reclaimed");
        assert!(
            lookalike.is_dir(),
            "a non-canonical trash suffix is not cache-owned"
        );

        let fresh_trash = namespace_root.join(format!("trash-{}", Uuid::new_v4()));
        std::fs::create_dir_all(fresh_trash.join("repo/.git")).expect("fresh trash payload");
        let fresh_mtime = std::fs::symlink_metadata(&fresh_trash)
            .expect("fresh trash metadata")
            .modified()
            .expect("fresh trash mtime");
        let removed = reap_stale_staging(
            root.path(),
            fresh_mtime + std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(60),
        )
        .expect("scan fresh trash residue");
        assert_eq!(removed, 0, "an in-flight deletion must survive the sweep");
        assert!(
            fresh_trash.is_dir(),
            "fresh trash residue is protected by the age fence"
        );
    }

    /// A `git clone` failure must not leave a staging wrapper behind.
    #[test]
    fn ensure_clone_cleans_up_staging_dir_on_clone_failure() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", dir.path());

        let bogus_source = dir.path().join("does-not-exist-as-a-repo");
        let result = ensure_clone(bogus_source.to_str().expect("utf8 path"));
        assert!(
            result.is_err(),
            "cloning a nonexistent local path must fail: {result:?}"
        );

        let namespace_root = staging_namespace_path(dir.path());
        let leftovers: Vec<_> = std::fs::read_dir(&namespace_root)
            .expect("read staging namespace")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|name| is_staging_wrapper_name(name))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a failed clone must not leave staging wrappers behind: {leftovers:?}"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }

    fn test_git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(
            status.success(),
            "git {args:?} failed in {}",
            repo.display()
        );
    }

    fn test_head_sha(repo: &Path) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        assert!(
            out.status.success(),
            "rev-parse failed in {}",
            repo.display()
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Issue #1644: `git fetch --prune` updates `refs/remotes/origin/*` but
    /// never advances the slot's checked-out HEAD, so every walk after the
    /// first ran against the HEAD frozen at clone time — an empty
    /// `{cursor}..HEAD` range that read as a clean completion. A re-`ensure`
    /// of an existing slot must leave the checkout AT the fetched tip.
    #[test]
    fn reensure_advances_checkout_to_fetched_tip() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", dir.path());

        let upstream = dir.path().join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        test_git(&upstream, &["init", "-q"]);
        test_git(&upstream, &["config", "user.email", "t@example.com"]);
        test_git(&upstream, &["config", "user.name", "T"]);
        std::fs::write(upstream.join("a.md"), "a\n").unwrap();
        test_git(&upstream, &["add", "a.md"]);
        test_git(&upstream, &["commit", "-q", "-m", "commit A"]);

        let url = upstream.to_str().expect("utf8 path");
        let slot = ensure_clone(url).expect("initial clone");
        assert_eq!(
            test_head_sha(&slot),
            test_head_sha(&upstream),
            "fresh clone starts at upstream HEAD"
        );

        // Upstream advances after the clone.
        std::fs::write(upstream.join("b.md"), "b\n").unwrap();
        test_git(&upstream, &["add", "b.md"]);
        test_git(&upstream, &["commit", "-q", "-m", "commit B"]);
        let upstream_tip = test_head_sha(&upstream);

        let slot2 = ensure_clone(url).expect("re-ensure existing slot");
        assert_eq!(slot2, slot, "same cache slot");
        assert_eq!(
            test_head_sha(&slot2),
            upstream_tip,
            "re-ensure must advance the checkout to the fetched tip \
             (issue #1644): a stale HEAD walks stale history"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }

    /// RAII scratch-root override: sets `KHIVE_GIT_DIGEST_SCRATCH_ROOT` and
    /// restores the previous value on drop, panic included — a bare
    /// `set_var`/`remove_var` pair leaks a deleted `TempDir` path into later
    /// tests when an assertion between them fails. Hold alongside the
    /// `ENV_MUTEX` guard.
    struct ScratchRootGuard {
        prev: Option<std::ffi::OsString>,
    }

    impl ScratchRootGuard {
        fn set(path: &Path) -> Self {
            let prev = std::env::var_os("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
            std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", path);
            Self { prev }
        }
    }

    impl Drop for ScratchRootGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", v),
                None => std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT"),
            }
        }
    }

    /// Every entry in a cache slot that came from a checkout: everything
    /// except `.git` and the cache's own `MARKER_FILE`, both of which this
    /// crate writes itself. Named exclusions rather than a dotfile rule, so a
    /// checked-out dotfile still counts as a materialized tree.
    fn worktree_entries(slot: &Path) -> Vec<String> {
        std::fs::read_dir(slot)
            .expect("read cache slot")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != ".git" && name != MARKER_FILE)
            .collect()
    }

    /// Issue #2104 defect 2: the slot is cloned `--filter=blob:none`, but a
    /// checkout backfills every blob reachable at HEAD and undoes the filter.
    /// Nothing reads this worktree — every consumer command is `rev-parse`,
    /// `log`, or `remote` — so the materialized tree is pure cost.
    ///
    /// The second half of this test is the one that matters. `--no-checkout`
    /// alone does not hold: the previous `advance_to_fetched_tip` ran
    /// `reset --hard`, which repopulates the worktree on the very next
    /// `ensure_clone` and gives the bytes straight back. So the assertion
    /// after the re-ensure is what fails if the ref update ever regresses to
    /// a reset, and the assertion on the fresh clone alone would not catch it.
    #[test]
    fn cache_slot_never_materializes_a_working_tree() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let _scratch = ScratchRootGuard::set(dir.path());

        let upstream = dir.path().join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        test_git(&upstream, &["init", "-q"]);
        test_git(&upstream, &["config", "user.email", "t@example.com"]);
        test_git(&upstream, &["config", "user.name", "T"]);
        std::fs::write(upstream.join("tracked.md"), "content\n").unwrap();
        test_git(&upstream, &["add", "tracked.md"]);
        test_git(&upstream, &["commit", "-q", "-m", "commit A"]);

        // The upstream itself HAS a working tree, so an empty result from
        // `worktree_entries` below is a real absence and not a helper that
        // always returns nothing.
        assert!(
            worktree_entries(&upstream).contains(&"tracked.md".to_string()),
            "control: the upstream must have a materialized tree, else the \
             assertions below prove nothing"
        );

        let url = upstream.to_str().expect("utf8 path");
        let slot = ensure_clone(url).expect("initial clone");
        assert_eq!(
            worktree_entries(&slot),
            Vec::<String>::new(),
            "a fresh cache slot must not check out a working tree"
        );
        assert_eq!(
            test_head_sha(&slot),
            test_head_sha(&upstream),
            "HEAD must still resolve without a checkout"
        );

        std::fs::write(upstream.join("second.md"), "more\n").unwrap();
        test_git(&upstream, &["add", "second.md"]);
        test_git(&upstream, &["commit", "-q", "-m", "commit B"]);
        let upstream_tip = test_head_sha(&upstream);

        let slot2 = ensure_clone(url).expect("re-ensure existing slot");
        assert_eq!(slot2, slot, "same cache slot");
        assert_eq!(
            test_head_sha(&slot2),
            upstream_tip,
            "advancing to the fetched tip must still work without a checkout"
        );
        assert_eq!(
            worktree_entries(&slot2),
            Vec::<String>::new(),
            "advancing the slot must move a ref, not reset a working tree: a \
             `reset --hard` here repopulates the tree and undoes the blob \
             filter on every pass"
        );
    }

    /// The `--no-checkout` clone and the ref-only advance stop a slot from
    /// BECOMING materialized. Neither un-materializes a slot that already is,
    /// so an installation upgraded across that change keeps every worktree it
    /// already had, on every slot, indefinitely — the existing-slot path
    /// fetches and moves a ref and touches a marker, and none of those removes
    /// a file.
    ///
    /// The fixture reproduces the legacy state by running the operation that
    /// produced it: `reset --hard` is exactly what `advance_to_fetched_tip`
    /// used to do. That makes this a test against the real prior behaviour
    /// rather than against a hand-built approximation of it.
    #[cfg(unix)] // migration is unix-only; see migrate_legacy_slot
    #[test]
    fn an_existing_slot_with_a_worktree_is_migrated_on_next_use() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let _scratch = ScratchRootGuard::set(dir.path());

        let upstream = dir.path().join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        test_git(&upstream, &["init", "-q"]);
        test_git(&upstream, &["config", "user.email", "t@example.com"]);
        test_git(&upstream, &["config", "user.name", "T"]);
        std::fs::write(upstream.join("tracked.md"), "content\n").unwrap();
        test_git(&upstream, &["add", "tracked.md"]);
        test_git(&upstream, &["commit", "-q", "-m", "commit A"]);

        let url = upstream.to_str().expect("utf8 path");
        let slot = ensure_clone(url).expect("initial clone");

        // Reproduce a pre-`--no-checkout` slot by running the operation that
        // used to create one.
        test_git(&slot, &["reset", "--hard"]);
        assert!(
            worktree_entries(&slot).contains(&"tracked.md".to_string()),
            "fixture control: the seeded slot must actually carry a worktree, \
             otherwise the migration assertion below passes vacuously"
        );
        assert!(
            slot.join(".git").join("index").exists(),
            "fixture control: the seeded slot must carry a populated index"
        );

        let slot2 = ensure_clone(url).expect("re-ensure the legacy slot");
        assert_eq!(slot2, slot, "same cache slot");
        assert_eq!(
            worktree_entries(&slot2),
            Vec::<String>::new(),
            "an existing slot carrying a worktree must be migrated on next \
             use; without that the blob saving arrives only when a slot \
             happens to be evicted or recloned"
        );
        assert!(
            !slot2.join(".git").join("index").exists(),
            "the index must go with the files, or every removed path reads as \
             a staged deletion"
        );
        assert_eq!(
            test_head_sha(&slot2),
            test_head_sha(&upstream),
            "migration must not damage the slot: HEAD still resolves"
        );
    }

    /// The repair path reaches legacy slots too, and it is a separate call
    /// site from the one above.
    ///
    /// Without this test the production call in `refetch_clone_locked` can be
    /// deleted or reordered while the ensure-path test stays green, which
    /// leaves repair-triggered legacy slots materialized — the exact defect
    /// the ensure-path test was written to catch, surviving in the path that
    /// test does not execute. Coverage of a migration belongs at every call
    /// site that can present the state, not once per migration.
    #[cfg(unix)] // migration is unix-only; see migrate_legacy_slot
    #[test]
    fn a_legacy_slot_reached_by_the_repair_path_is_migrated_too() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let _scratch = ScratchRootGuard::set(dir.path());

        let upstream = dir.path().join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        test_git(&upstream, &["init", "-q"]);
        test_git(&upstream, &["config", "user.email", "t@example.com"]);
        test_git(&upstream, &["config", "user.name", "T"]);
        std::fs::write(upstream.join("tracked.md"), "content\n").unwrap();
        test_git(&upstream, &["add", "tracked.md"]);
        test_git(&upstream, &["commit", "-q", "-m", "commit A"]);

        let url = upstream.to_str().expect("utf8 path");
        let slot = ensure_clone(url).expect("initial clone");

        // Same seeding operation as the ensure-path test: the thing that
        // actually produced these slots before `--no-checkout`.
        test_git(&slot, &["reset", "--hard"]);
        assert!(
            worktree_entries(&slot).contains(&"tracked.md".to_string()),
            "fixture control: the seeded slot must actually carry a worktree, \
             otherwise the assertion below passes vacuously"
        );
        assert!(
            slot.join(".git").join("index").exists(),
            "fixture control: the seeded slot must carry a populated index"
        );

        let slot2 = refetch_clone(url).expect("refetch the legacy slot");
        assert_eq!(slot2, slot, "same cache slot");
        assert_eq!(
            worktree_entries(&slot2),
            Vec::<String>::new(),
            "the repair path must migrate a legacy slot as well; it reaches \
             the same pre-`--no-checkout` slots the ensure path does"
        );
        assert!(
            !slot2.join(".git").join("index").exists(),
            "the index must not survive the migration on this path either"
        );
        assert_eq!(
            test_head_sha(&slot2),
            test_head_sha(&upstream),
            "migration must not damage the slot: HEAD still resolves"
        );
    }

    #[test]
    fn evict_lru_removes_oldest_past_repo_cap() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", dir.path());
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS", "1");
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", "1000000000");

        let root = dir.path();
        let old = make_owned_entry(root, "1111111111111111", true);
        // Ensure a real mtime gap.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let new = make_owned_entry(root, "2222222222222222", true);

        evict_lru(root, &new).expect("evict");

        assert!(!old.exists(), "the older clone must be evicted");
        assert!(new.exists(), "the kept clone must survive");

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS");
        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES");
    }

    /// Issue #960: a cache mutation that FAILS must still leave the caps
    /// enforced. A failed `refetch_clone` returns before its success-path
    /// `evict_lru`, and under concurrency a sibling eviction pass can defer
    /// this slot (its lock is held) — so without a post-release cap pass the
    /// caps stay exceeded with nothing scheduled to correct them.
    /// `finish_mutation` runs `enforce_caps` once the lock is free. This pins
    /// the settled-state invariant the concurrent case also relies on: two
    /// owned slots over a repo cap of 1, a failed refetch of one, and
    /// afterward exactly one owned slot remains.
    #[test]
    fn a_failed_mutation_enforces_caps_over_the_settled_set() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", dir.path());
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS", "1");
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", "1000000000");

        let root = dir.path();
        // Two owned slots present, one over the repo cap of 1. The slot we
        // will fail to refetch is the newer one; the older sibling is the LRU
        // eviction victim, showing the failed mutation enforced the cap over a
        // slot it was not itself operating on.
        let url_victim = "https://example.com/lru-victim.git";
        let url_target = "https://example.com/refetch-target.git";
        let key_victim = cache_key(url_victim);
        let key_target = cache_key(url_target);
        assert_ne!(
            key_victim, key_target,
            "distinct urls must map to distinct slots"
        );

        let victim = make_owned_entry(root, &key_victim, true);
        // Ensure a real mtime gap so `victim` is unambiguously the LRU.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let target = make_owned_entry(root, &key_target, true);

        // `target`'s `.git` is an empty directory, not a real repository, so
        // `git fetch --refetch` fails deterministically with no network. The
        // mutation therefore returns Err before its own eviction pass.
        let result = refetch_clone(url_target);
        assert!(
            result.is_err(),
            "refetch of a slot with no valid git repo must fail: {result:?}"
        );

        // The failed mutation must nonetheless have enforced the caps.
        let owned: Vec<_> = std::fs::read_dir(root)
            .expect("read scratch root")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| is_owned_entry(p))
            .collect();
        assert_eq!(
            owned.len(),
            1,
            "a failed mutation must leave the repo cap enforced, found: {owned:?}"
        );
        assert!(
            target.exists(),
            "the newer (refetched) slot must survive as the non-LRU entry"
        );
        assert!(
            !victim.exists(),
            "the older sibling must be evicted to satisfy the repo cap"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS");
        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES");
    }

    #[test]
    fn evict_lru_only_touches_children_of_root() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS", "5");
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", "1000000000");

        let root = dir.path().join("scratch-root");
        std::fs::create_dir_all(&root).unwrap();
        let kept = make_owned_entry(&root, "3333333333333333", true);

        evict_lru(&root, &kept).expect("evict");
        assert!(kept.exists());

        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS");
        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES");
    }

    #[test]
    fn evict_lru_never_removes_a_foreign_directory_under_root() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        // Cap of 0 repos: without ownership filtering this would previously
        // have wiped out every child of root, including operator data.
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS", "0");
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", "0");

        let root = dir.path().join("scratch-root");
        std::fs::create_dir_all(&root).unwrap();
        let foreign = root.join("not-a-cache-entry");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("important.txt"), b"do not delete me").unwrap();
        let kept = make_owned_entry(&root, "4444444444444444", true);

        evict_lru(&root, &kept).expect("evict");

        assert!(
            foreign.exists(),
            "a directory that doesn't look like a cache slot must survive eviction"
        );
        assert!(
            foreign.join("important.txt").exists(),
            "foreign directory contents must be untouched"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS");
        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES");
    }

    #[test]
    fn evict_lru_does_not_grow_registry_for_unrelated_scratch_root_children() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("scratch-root");
        std::fs::create_dir_all(&root).unwrap();
        let kept = make_owned_entry(&root, "4444444444444444", true);

        for index in 0..32 {
            std::fs::create_dir_all(root.join(format!("operator-data-{index}"))).unwrap();
        }

        let baseline = slot_lock_registry_len();
        evict_lru(&root, &kept).expect("evict");
        assert_eq!(
            slot_lock_registry_len(),
            baseline,
            "unrelated scratch-root children must not allocate slot locks"
        );
    }

    #[test]
    fn evict_lru_never_removes_an_owned_looking_dir_missing_the_marker() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS", "0");
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", "0");

        let root = dir.path().join("scratch-root");
        std::fs::create_dir_all(&root).unwrap();
        // Has a .git dir and a valid cache-key-shaped name, but no marker --
        // e.g. a clone that failed after `clone()` but before `touch()`.
        let no_marker = make_owned_entry(&root, "5555555555555555", false);
        let kept = make_owned_entry(&root, "6666666666666666", true);

        let baseline = slot_lock_registry_len();
        evict_lru(&root, &kept).expect("evict");

        assert!(
            no_marker.exists(),
            "an owned-looking directory without the marker must survive eviction"
        );
        assert_eq!(
            slot_lock_registry_len(),
            baseline,
            "an unowned cache-shaped child must not allocate a slot lock"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS");
        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES");
    }

    /// The install path must fail closed when a foreign (unowned) directory
    /// occupies the slot pathname: staging plus a single `rename` refuses a
    /// non-empty destination, so the foreign bytes survive and no ownership
    /// marker is written. This is the second half of `ensure_clone_locked`'s
    /// TOCTOU regression: a slot swapped for an empty directory between the
    /// ownership decision and the fetch must (a) fail `revalidate_owned_slot`
    /// and (b) even if a fetch were issued anyway, fail on the exact path
    /// rather than discovering upward into an ancestor repository. The
    /// ancestor here is a real git repository containing the cache root —
    /// the shape under which `-C` discovery would have fetched into it.
    #[test]
    fn a_swapped_slot_is_refused_and_never_reaches_an_ancestor_repo() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");

        // A fetchable upstream, so that an upward-discovering `git -C` fetch
        // from inside the ancestor would SUCCEED (creating FETCH_HEAD) —
        // without it the old `-C` form fails for the wrong reason and this
        // test could not distinguish the fixed form from the broken one.
        let upstream = dir.path().join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        test_git(&upstream, &["init", "-q"]);
        test_git(&upstream, &["config", "user.email", "t@example.com"]);
        test_git(&upstream, &["config", "user.name", "T"]);
        std::fs::write(upstream.join("tracked.md"), "content\n").unwrap();
        test_git(&upstream, &["add", "tracked.md"]);
        test_git(&upstream, &["commit", "-q", "-m", "commit A"]);

        // Ancestor repository enclosing the cache root, with that upstream
        // configured as origin.
        let ancestor = dir.path().join("ancestor");
        std::fs::create_dir_all(&ancestor).unwrap();
        test_git(&ancestor, &["init", "-q"]);
        test_git(
            &ancestor,
            &["remote", "add", "origin", upstream.to_str().expect("utf8")],
        );
        let root = ancestor.join("cache-root");
        std::fs::create_dir_all(&root).unwrap();
        let _scratch = ScratchRootGuard::set(&root);

        // The swapped-in slot: an empty directory where an owned clone stood.
        let slot = root.join("swapped-slot");
        std::fs::create_dir_all(&slot).unwrap();

        // (a) descriptor-bound revalidation refuses it.
        assert!(
            matches!(
                revalidate_owned_slot(&slot),
                Err(CacheError::UnsafeToReplace(_))
            ),
            "an empty directory at the slot pathname must fail revalidation"
        );

        // (b) even a command bound to the hostile directory itself (the
        // test-only constructor skips the ownership check layer (a) proves)
        // errors on the missing relative `.git` instead of discovering
        // upward into the ancestor.
        #[cfg(unix)]
        {
            assert!(
                fetch(&slot, &ValidatedSlot::for_test(&slot)).is_err(),
                "fetch against a vanished slot must fail loudly"
            );
        }
        assert!(
            !ancestor.join(".git").join("FETCH_HEAD").exists(),
            "the ancestor repository must be untouched by the failed fetch"
        );
    }

    /// The descriptor layer binds the git command to the directory OBJECT
    /// validated by `revalidate_owned_slot`, not to the pathname: after
    /// validation, the slot pathname is swapped for a symlink pointing at an
    /// ancestor repository — the shape an absolute `--git-dir` would follow.
    /// The bound fetch must land in the validated (renamed-aside) directory
    /// and never touch the ancestor.
    #[cfg(unix)]
    #[test]
    fn a_bound_command_follows_the_validated_object_not_the_swapped_pathname() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");

        let upstream = dir.path().join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        test_git(&upstream, &["init", "-q"]);
        test_git(&upstream, &["config", "user.email", "t@example.com"]);
        test_git(&upstream, &["config", "user.name", "T"]);
        std::fs::write(upstream.join("tracked.md"), "content\n").unwrap();
        test_git(&upstream, &["add", "tracked.md"]);
        test_git(&upstream, &["commit", "-q", "-m", "commit A"]);
        let url = upstream.to_str().expect("utf8");

        // Ancestor repository with the same fetchable origin, so that a
        // symlink-following fetch would SUCCEED into it — without it the
        // broken form fails for the wrong reason.
        let ancestor = dir.path().join("ancestor");
        std::fs::create_dir_all(&ancestor).unwrap();
        test_git(&ancestor, &["init", "-q"]);
        test_git(&ancestor, &["remote", "add", "origin", url]);
        let root = ancestor.join("cache-root");
        std::fs::create_dir_all(&root).unwrap();
        let _scratch = ScratchRootGuard::set(&root);

        // A genuine owned slot.
        let slot = root.join("owned-slot");
        clone(url, &slot).expect("clone slot");
        std::fs::write(slot.join(MARKER_FILE), b"").expect("marker");

        let validated = revalidate_owned_slot(&slot).expect("owned slot validates");

        // Post-validation swap: the pathname now points at the ancestor.
        let moved = root.join("owned-slot-moved");
        std::fs::rename(&slot, &moved).expect("move validated dir aside");
        std::os::unix::fs::symlink(&ancestor, &slot).expect("symlink swap");

        fetch(&slot, &validated).expect("bound fetch follows the validated object");

        assert!(
            !ancestor.join(".git").join("FETCH_HEAD").exists(),
            "the ancestor repository must never receive the fetch"
        );
        assert!(
            moved.join(".git").join("FETCH_HEAD").exists(),
            "the fetch must land in the directory that was validated"
        );
    }

    /// The descriptor layer binds git to the validated `.git` OBJECT, not to
    /// the name `.git`: after validation, the slot's `.git` CHILD ENTRY is
    /// swapped for a symlink pointing at an ancestor repository's `.git` — the
    /// shape a re-resolved relative `--git-dir .git` would follow. Binding the
    /// parent slot alone left this open (the parent pathname is unchanged, so
    /// only the child entry moves); the fetch must land in the validated
    /// `.git` and never touch the ancestor.
    #[cfg(unix)]
    #[test]
    fn a_bound_command_follows_the_validated_git_object_not_a_swapped_git_child() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");

        let upstream = dir.path().join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        test_git(&upstream, &["init", "-q"]);
        test_git(&upstream, &["config", "user.email", "t@example.com"]);
        test_git(&upstream, &["config", "user.name", "T"]);
        std::fs::write(upstream.join("tracked.md"), "content\n").unwrap();
        test_git(&upstream, &["add", "tracked.md"]);
        test_git(&upstream, &["commit", "-q", "-m", "commit A"]);
        let url = upstream.to_str().expect("utf8");

        // Ancestor repository with the same fetchable origin, so that a
        // symlink-following fetch would SUCCEED into it — without it the
        // broken form fails for the wrong reason.
        let ancestor = dir.path().join("ancestor");
        std::fs::create_dir_all(&ancestor).unwrap();
        test_git(&ancestor, &["init", "-q"]);
        test_git(&ancestor, &["remote", "add", "origin", url]);
        let root = ancestor.join("cache-root");
        std::fs::create_dir_all(&root).unwrap();
        let _scratch = ScratchRootGuard::set(&root);

        // A genuine owned slot.
        let slot = root.join("owned-slot");
        clone(url, &slot).expect("clone slot");
        std::fs::write(slot.join(MARKER_FILE), b"").expect("marker");

        let validated = revalidate_owned_slot(&slot).expect("owned slot validates");

        // Post-validation swap of the CHILD ENTRY: move the real `.git` aside
        // and point the name `.git` at the ancestor's `.git`. The slot
        // pathname itself is untouched, so a parent-only binding still lands
        // here and re-resolves `.git` by name.
        let real_git = slot.join(".git");
        let moved_git = slot.join(".git-moved");
        std::fs::rename(&real_git, &moved_git).expect("move validated .git aside");
        std::os::unix::fs::symlink(ancestor.join(".git"), &real_git).expect("symlink .git swap");

        fetch(&slot, &validated).expect("bound fetch follows the validated .git object");

        assert!(
            !ancestor.join(".git").join("FETCH_HEAD").exists(),
            "the ancestor repository must never receive the fetch"
        );
        assert!(
            moved_git.join("FETCH_HEAD").exists(),
            "the fetch must land in the .git object that was validated"
        );
    }

    /// no-re-read guarantee: once the slot state is decided `Absent`, a
    /// foreign directory appearing at the pathname must not be fetched
    /// into, overwritten, or claimed.
    #[test]
    fn install_fresh_clone_refuses_a_foreign_occupied_slot() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let _scratch = ScratchRootGuard::set(dir.path());

        let upstream = dir.path().join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        test_git(&upstream, &["init", "-q"]);
        test_git(&upstream, &["config", "user.email", "t@example.com"]);
        test_git(&upstream, &["config", "user.name", "T"]);
        std::fs::write(upstream.join("tracked.md"), "content\n").unwrap();
        test_git(&upstream, &["add", "tracked.md"]);
        test_git(&upstream, &["commit", "-q", "-m", "commit A"]);
        let url = upstream.to_str().expect("utf8 path");

        // A foreign process's directory at the slot pathname: it has a
        // `.git` but no ownership marker, plus a sentinel byte the
        // assertions below prove survives.
        let root = scratch_root();
        prepare_cache_root(&root).expect("cache root");
        let repo_dir = root.join(cache_key(url));
        std::fs::create_dir_all(repo_dir.join(".git")).unwrap();
        std::fs::write(repo_dir.join("foreign.txt"), "foreign\n").unwrap();
        assert!(
            !is_owned_entry(&repo_dir),
            "fixture control: the occupying directory must be unowned"
        );

        install_fresh_clone(url, &root, &repo_dir, clone_max_bytes())
            .expect_err("install into an occupied foreign pathname must fail");
        assert_eq!(
            std::fs::read_to_string(repo_dir.join("foreign.txt")).expect("sentinel readable"),
            "foreign\n",
            "foreign bytes must survive a refused install"
        );
        assert!(
            !repo_dir.join(MARKER_FILE).exists(),
            "a refused install must never write the ownership marker"
        );
    }

    #[test]
    fn is_owned_entry_rejects_non_cache_shapes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        // Wrong length.
        let short = root.join("abc123");
        std::fs::create_dir_all(short.join(".git")).unwrap();
        std::fs::write(short.join(MARKER_FILE), b"").unwrap();
        assert!(!is_owned_entry(&short));

        // Uppercase hex (cache_key is always lowercase).
        let upper = root.join("ABCDEF0123456789");
        std::fs::create_dir_all(upper.join(".git")).unwrap();
        std::fs::write(upper.join(MARKER_FILE), b"").unwrap();
        assert!(!is_owned_entry(&upper));

        // Right shape but missing .git.
        let no_git = root.join("7777777777777777");
        std::fs::create_dir_all(&no_git).unwrap();
        std::fs::write(no_git.join(MARKER_FILE), b"").unwrap();
        assert!(!is_owned_entry(&no_git));

        let owned = make_owned_entry(root, "8888888888888888", true);
        assert!(is_owned_entry(&owned));
    }

    #[test]
    fn dir_size_sums_file_bytes_recursively() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), b"12345").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), b"1234567890").unwrap();
        assert_eq!(dir_size(dir.path()).unwrap(), 15);
    }

    /// PR #847: walk root vanishing must error, never launder to `Ok(0)`.
    #[test]
    fn dir_size_errors_when_the_root_itself_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        let err = dir_size(&missing).expect_err("a missing root must error, not size to 0");
        assert!(
            matches!(&err, CacheError::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
            "expected CacheError::Io(NotFound), got {err:?}"
        );
    }

    /// `keep` vanishing must propagate, not be treated as an empty slot.
    #[test]
    fn evict_lru_errors_when_keep_itself_is_missing() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS", "5");
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", "1000000000");

        let root = dir.path().join("scratch-root");
        std::fs::create_dir_all(&root).unwrap();
        let missing_keep = root.join("0000000000000000");

        let err = evict_lru(&root, &missing_keep).expect_err("a missing keep root must error");
        assert!(
            matches!(&err, CacheError::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
            "expected CacheError::Io(NotFound), got {err:?}"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS");
        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES");
    }

    /// Issue #842 macOS ENOENT flake family: a descendant disappearing
    /// mid-walk must shrink the total, not abort with `NotFound`. See
    /// crates/khive-pack-git/docs/api/cache.md#test-module-notes.
    #[test]
    fn dir_size_tolerates_a_subdirectory_removed_mid_walk() {
        for _ in 0..200 {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().to_path_buf();
            let victim = root.join("victim");
            std::fs::create_dir_all(&victim).unwrap();
            for i in 0..64 {
                std::fs::write(victim.join(format!("f{i}.txt")), b"0123456789").unwrap();
            }
            // A wide fan of siblings so the walk still has entries left on
            // its stack (and is plausibly still inside `victim`) at the
            // instant the other thread deletes it.
            for i in 0..64 {
                let sibling = root.join(format!("sibling{i}"));
                std::fs::create_dir_all(&sibling).unwrap();
                std::fs::write(sibling.join("s.txt"), b"0123456789").unwrap();
            }

            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let walk_root = root.clone();
            let walk_barrier = barrier.clone();
            let walker = std::thread::spawn(move || {
                walk_barrier.wait();
                dir_size(&walk_root)
            });
            let delete_victim = victim.clone();
            let deleter = std::thread::spawn(move || {
                barrier.wait();
                let _ = std::fs::remove_dir_all(&delete_victim);
            });

            let result = walker.join().expect("walker thread");
            deleter.join().expect("deleter thread");

            assert!(
                result.is_ok(),
                "dir_size must tolerate a subdirectory vanishing mid-walk, got {result:?}"
            );
        }
    }

    /// Companion to the test above (PR #847): the walk root itself
    /// vanishing must error, not tolerate like a descendant. See
    /// crates/khive-pack-git/docs/api/cache.md#test-module-notes.
    #[test]
    fn dir_size_errors_when_the_root_is_removed_mid_walk() {
        let mut saw_error = false;
        for _ in 0..500 {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().join("slot");
            std::fs::create_dir_all(&root).unwrap();

            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let walk_root = root.clone();
            let walk_barrier = barrier.clone();
            let walker = std::thread::spawn(move || {
                walk_barrier.wait();
                dir_size(&walk_root)
            });
            let delete_root = root.clone();
            let deleter = std::thread::spawn(move || {
                barrier.wait();
                let _ = std::fs::remove_dir(&delete_root);
            });

            let result = walker.join().expect("walker thread");
            deleter.join().expect("deleter thread");

            match result {
                Ok(_) => continue, // walker won the race this round; try again
                Err(CacheError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    saw_error = true;
                }
                Err(e) => panic!("unexpected error kind from a vanished root: {e:?}"),
            }
        }
        assert!(
            saw_error,
            "root-vanish-mid-walk race was never hit across 500 iterations; \
             widen the fixture or investigate the barrier timing"
        );
    }

    // ── issue #765: refetch/reclone repair primitives ──────────────────────

    fn git(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A real local repo usable as a `canonical_url` (git accepts a plain
    /// filesystem path as a clone/fetch source).
    fn init_origin_with_one_commit(repo: &Path) {
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test User"]);
        std::fs::write(repo.join("a.txt"), b"hello").unwrap();
        git(repo, &["add", "a.txt"]);
        git(repo, &["commit", "-q", "-m", "initial"]);
    }

    fn add_commit(repo: &Path, rel: &str, contents: &str, message: &str) {
        std::fs::write(repo.join(rel), contents).unwrap();
        git(repo, &["add", rel]);
        git(repo, &["commit", "-q", "-m", message]);
    }

    fn head_sha(repo: &Path) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Primary #765 acceptance path — see
    /// crates/khive-pack-git/docs/api/cache.md#test-module-notes.
    #[test]
    fn refetch_clone_updates_an_existing_slot_to_the_remote_tip() {
        let _guard = ENV_MUTEX.blocking_lock();
        let scratch = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

        let origin_dir = tempfile::tempdir().expect("tempdir");
        init_origin_with_one_commit(origin_dir.path());
        let canonical = origin_dir.path().to_str().unwrap();

        let first = ensure_clone(canonical).expect("initial ensure_clone");
        let before = head_sha(&first);

        add_commit(origin_dir.path(), "b.txt", "world", "second");
        let expected_tip = head_sha(origin_dir.path());
        assert_ne!(before, expected_tip, "origin must have moved on");

        let repaired = refetch_clone(canonical).expect("refetch_clone");
        assert_eq!(repaired, first, "refetch repairs the same cache slot path");
        git(&repaired, &["show", &format!("{expected_tip}:b.txt")]);

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }

    /// Remediation (issue #765) — see
    /// crates/khive-pack-git/docs/api/cache.md#test-module-notes.
    #[test]
    fn refetch_clone_over_cap_cleanup_never_deletes_an_unproven_slot() {
        let _guard = ENV_MUTEX.blocking_lock();
        let scratch = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

        let origin_dir = tempfile::tempdir().expect("tempdir");
        init_origin_with_one_commit(origin_dir.path());
        let canonical = origin_dir.path().to_str().unwrap();

        let slot = ensure_clone(canonical).expect("initial ensure_clone");
        // Simulate a slot the ownership guard cannot prove it owns (e.g. a
        // crash between a prior clone/fetch and `touch`, or a foreign
        // directory occupying this exact cache-key path) by removing the
        // marker `touch` would normally have written.
        std::fs::remove_file(slot.join(".khive-last-used")).expect("remove marker");

        std::env::set_var("KHIVE_GIT_DIGEST_CLONE_MAX_BYTES", "1");
        let err = refetch_clone(canonical).expect_err("refetch must report the ownership error");
        assert!(
            matches!(err, CacheError::UnsafeToReplace(_)),
            "expected UnsafeToReplace (the cleanup's ownership failure, propagated), got {err:?}"
        );
        assert!(
            slot.exists(),
            "a slot the ownership guard cannot prove it owns must survive over-cap cleanup"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_CLONE_MAX_BYTES");
        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }

    /// Remediation (issue #765 follow-up PR #788) — see
    /// crates/khive-pack-git/docs/api/cache.md#test-module-notes.
    #[test]
    fn refetch_clone_refuses_a_markerless_slot_under_the_cap() {
        let _guard = ENV_MUTEX.blocking_lock();
        let scratch = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

        let origin_dir = tempfile::tempdir().expect("tempdir");
        init_origin_with_one_commit(origin_dir.path());
        let canonical = origin_dir.path().to_str().unwrap();

        let slot = ensure_clone(canonical).expect("initial ensure_clone");
        let sentinel_sha = head_sha(&slot);
        std::fs::remove_file(slot.join(MARKER_FILE)).expect("remove marker");

        // The origin moves on -- if the ownership guard failed to fire and
        // a real fetch ran, the slot's HEAD would follow.
        add_commit(origin_dir.path(), "b.txt", "world", "second");

        let err = refetch_clone(canonical)
            .expect_err("a markerless slot must be refused before any fetch runs");
        assert!(
            matches!(err, CacheError::UnsafeToReplace(_)),
            "expected UnsafeToReplace, got {err:?}"
        );
        assert_eq!(
            head_sha(&slot),
            sentinel_sha,
            "no fetch must have run against the markerless slot"
        );
        assert!(
            !slot.join(MARKER_FILE).exists(),
            "a refused refetch must never (re)write the ownership marker"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }

    #[test]
    fn refetch_clone_errors_when_no_slot_exists() {
        let _guard = ENV_MUTEX.blocking_lock();
        let scratch = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

        let err = refetch_clone("https://example.invalid/never-cloned/repo")
            .expect_err("no slot exists yet");
        assert!(
            matches!(err, CacheError::Git(_)),
            "expected CacheError::Git, got {err:?}"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }

    /// Issue #7: a "no cache slot exists" error must not leak an embedded
    /// credential or query-string token from the caller-supplied URL.
    #[test]
    fn refetch_clone_no_slot_error_redacts_credential_bearing_url() {
        let _guard = ENV_MUTEX.blocking_lock();
        let scratch = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

        let err =
            refetch_clone("https://user:tok3n@example.invalid/never-cloned/repo?token=SECRET")
                .expect_err("no slot exists yet");
        let msg = err.to_string();
        assert!(
            !msg.contains("tok3n") && !msg.contains("SECRET"),
            "issue #7: refetch-no-slot error must not leak embedded credentials/token: {msg}"
        );
        assert!(
            msg.contains("example.invalid"),
            "redaction must preserve the host for diagnosability: {msg}"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }

    /// Issue #7: a `git clone` failure's error message must not leak an
    /// embedded credential or query-string token from the caller-supplied
    /// URL. Port 1 is a reserved low port unlikely to have anything
    /// listening, so the clone fails fast on connection refusal rather than
    /// waiting on a real network timeout.
    #[test]
    fn ensure_clone_failure_message_redacts_credential_bearing_url() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", dir.path());

        let result = ensure_clone("https://user:tok3n@127.0.0.1:1/org/repo?token=SECRET");
        let err = result.expect_err("clone against a closed port must fail");
        let msg = err.to_string();
        assert!(
            !msg.contains("tok3n") && !msg.contains("SECRET"),
            "issue #7: clone failure message must not leak embedded credentials/token: {msg}"
        );
        assert!(
            msg.contains("127.0.0.1"),
            "redaction must preserve the host for diagnosability: {msg}"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }

    /// #765's fallback path — see
    /// crates/khive-pack-git/docs/api/cache.md#test-module-notes.
    #[test]
    fn reclone_replaces_a_slot_whose_refetch_cannot_succeed() {
        let _guard = ENV_MUTEX.blocking_lock();
        let scratch = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

        let origin_dir = tempfile::tempdir().expect("tempdir");
        init_origin_with_one_commit(origin_dir.path());
        let canonical = origin_dir.path().to_str().unwrap();

        let slot = ensure_clone(canonical).expect("initial ensure_clone");
        // Break the slot's own remote so `fetch --refetch origin` fails --
        // standing in for a corrupt slot that cannot self-repair via refetch.
        git(
            &slot,
            &[
                "remote",
                "set-url",
                "origin",
                "/nonexistent/path/does-not-exist",
            ],
        );
        assert!(matches!(refetch_clone(canonical), Err(CacheError::Git(_))));

        let recloned = reclone(canonical).expect("reclone");
        assert_eq!(recloned, slot, "reclone reinstalls at the same slot path");
        assert_eq!(head_sha(&recloned), head_sha(origin_dir.path()));
        // The fresh clone's own remote points back at the canonical URL, not
        // the broken one the corrupt slot had.
        let out = Command::new("git")
            .arg("-C")
            .arg(&recloned)
            .args(["remote", "get-url", "origin"])
            .output()
            .expect("remote get-url");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            canonical,
            "reclone must re-point origin at canonical_url, not the broken remote"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }

    /// Ownership guard (ADR-088 Amendment 1 / PR #761) — see
    /// crates/khive-pack-git/docs/api/cache.md#test-module-notes.
    #[test]
    fn reclone_refuses_to_replace_a_foreign_looking_directory() {
        let _guard = ENV_MUTEX.blocking_lock();
        let scratch = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

        let origin_dir = tempfile::tempdir().expect("tempdir");
        init_origin_with_one_commit(origin_dir.path());
        let canonical = origin_dir.path().to_str().unwrap();
        let key = cache_key(canonical);
        let foreign = scratch.path().join(&key);
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("important.txt"), b"do not delete me").unwrap();

        let err = reclone(canonical).expect_err("foreign directory must be refused");
        assert!(
            matches!(err, CacheError::UnsafeToReplace(_)),
            "expected UnsafeToReplace, got {err:?}"
        );
        assert!(
            foreign.join("important.txt").exists(),
            "foreign directory contents must survive a refused reclone"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }

    /// No slot exists yet: `reclone` simply installs a fresh clone.
    #[test]
    fn reclone_installs_fresh_when_no_slot_exists_yet() {
        let _guard = ENV_MUTEX.blocking_lock();
        let scratch = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

        let origin_dir = tempfile::tempdir().expect("tempdir");
        init_origin_with_one_commit(origin_dir.path());
        let canonical = origin_dir.path().to_str().unwrap();

        let recloned = reclone(canonical).expect("reclone with no prior slot");
        assert_eq!(head_sha(&recloned), head_sha(origin_dir.path()));

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }

    /// Remediation (issue #765) — see
    /// crates/khive-pack-git/docs/api/cache.md#test-module-notes.
    #[test]
    fn ensure_clone_refuses_a_markerless_git_directory_at_the_cache_key_path() {
        let _guard = ENV_MUTEX.blocking_lock();
        let scratch = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

        let canonical = "https://example.invalid/lookalike/repo";
        let key = cache_key(canonical);
        let lookalike = scratch.path().join(&key);
        std::fs::create_dir_all(&lookalike).unwrap();
        init_origin_with_one_commit(&lookalike);
        std::fs::write(lookalike.join("sentinel.txt"), b"do not delete me").unwrap();
        let sentinel_sha = head_sha(&lookalike);

        let err = ensure_clone(canonical).expect_err("markerless lookalike must be refused");
        assert!(
            matches!(err, CacheError::UnsafeToReplace(_)),
            "expected UnsafeToReplace, got {err:?}"
        );

        assert!(
            lookalike.join("sentinel.txt").exists(),
            "sentinel operator data must survive a refused ensure_clone"
        );
        assert_eq!(
            head_sha(&lookalike),
            sentinel_sha,
            "the lookalike repository's own history must be untouched (no fetch ran)"
        );
        assert!(
            !lookalike.join(MARKER_FILE).exists(),
            "a refused ensure_clone must never write the ownership marker either"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }

    /// Same guard, symlink variant.
    #[cfg(unix)]
    #[test]
    fn ensure_clone_refuses_a_symlink_at_the_cache_key_path() {
        let _guard = ENV_MUTEX.blocking_lock();
        let scratch = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

        let canonical = "https://example.invalid/symlink-lookalike/repo";
        let key = cache_key(canonical);
        let link_path = scratch.path().join(&key);

        let target = tempfile::tempdir().expect("symlink target");
        make_owned_entry(target.path(), "9999999999999999", true);
        let real_owned = target.path().join("9999999999999999");
        std::fs::write(real_owned.join("sentinel.txt"), b"do not delete me").unwrap();

        std::os::unix::fs::symlink(&real_owned, &link_path).expect("create symlink");

        let err = ensure_clone(canonical).expect_err("symlink lookalike must be refused");
        assert!(
            matches!(err, CacheError::UnsafeToReplace(_)),
            "expected UnsafeToReplace, got {err:?}"
        );
        assert!(
            real_owned.join("sentinel.txt").exists(),
            "the symlink target's sentinel data must survive a refused ensure_clone"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }

    /// Blocking-finding regression: an owned cache slot deleted through
    /// `remove_owned_entry` (LRU eviction / repair over-cap cleanup) must
    /// actually disappear, and a directory that is NOT a proven owned slot
    /// -- even one an external writer swapped in at the exact cache-key
    /// path after the caller last checked -- must never be deleted by the
    /// fd-verified path either.
    #[test]
    fn remove_owned_entry_deletes_a_genuinely_owned_slot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let owned = make_owned_entry(root, "aaaaaaaaaaaaaaaa", true);
        std::fs::write(owned.join("payload.txt"), b"clone contents").unwrap();

        remove_owned_entry(root, &owned).expect("remove owned slot");
        assert!(!owned.exists(), "an owned slot must be deleted");
    }

    #[cfg(unix)]
    #[test]
    fn remove_owned_entry_refuses_a_symlink_planted_at_the_cache_key_path_after_the_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let target = tempfile::tempdir().expect("symlink target");
        std::fs::write(target.path().join("victim.txt"), b"do not delete me").unwrap();

        // `remove_owned_entry`'s own top-level `is_owned_entry` check cannot
        // be satisfied by a bare symlink (it is filtered out before this
        // function is ever reached in the real callers), so this test drives
        // the fd-verified path directly to prove it independently refuses a
        // symlink even if some future caller skipped that earlier gate.
        let repo_dir = root.join("bbbbbbbbbbbbbbbb");
        std::os::unix::fs::symlink(target.path(), &repo_dir).expect("plant symlink");

        let err = delete_verified_owned_entry(root, &repo_dir)
            .expect_err("a symlink at the cache-key path must be refused");
        assert!(matches!(err, CacheError::UnsafeToReplace(_)));
        assert!(
            target.path().join("victim.txt").exists(),
            "the symlink target's contents must survive a refused deletion"
        );
    }

    // ── issue #805: same-key mutation serialization ────────────────────────

    /// `slot_lock` must serialize a *repeated* lookup of the same cache key
    /// (both calls return handles to the same underlying `Mutex`) while
    /// leaving a distinct key completely unaffected -- the acceptance
    /// criterion from issue #805 ("serialize per-key without serializing
    /// distinct keys").
    #[test]
    fn slot_lock_serializes_same_key_but_not_distinct_keys() {
        let _env_guard = ENV_MUTEX.blocking_lock();
        let key_a = "abcdef0123456789";
        let key_b = "fedcba9876543210";

        let lock_a1 = slot_lock(key_a);
        let guard = lock_a1.lock().expect("lock key_a");

        let lock_a2 = slot_lock(key_a);
        assert!(
            lock_a2.try_lock().is_err(),
            "a second lookup of the same cache key must observe the first as held"
        );

        let lock_b = slot_lock(key_b);
        assert!(
            lock_b.try_lock().is_ok(),
            "locking a distinct cache key must never be blocked by another key's held lock"
        );

        drop(guard);
        drop(lock_a1);

        let guard = lock_a2.lock().expect("re-lock key_a");
        let lock_a3 = slot_lock(key_a);
        assert!(
            lock_a3.try_lock().is_err(),
            "dropping one handle must not replace the lock while another handle still exists"
        );
        drop(guard);
    }

    #[test]
    fn released_distinct_slot_locks_do_not_grow_the_registry() {
        let _env_guard = ENV_MUTEX.blocking_lock();
        let baseline = slot_lock_registry_len();
        let baseline_capacity = slot_lock_registry_capacity();
        let locks: Vec<_> = (0..64)
            .map(|index| slot_lock(&format!("released-distinct-key-{index}")))
            .collect();

        assert_eq!(
            slot_lock_registry_len(),
            baseline + locks.len(),
            "live handles must remain registered"
        );
        drop(locks);
        assert_eq!(
            slot_lock_registry_len(),
            baseline,
            "released handles must remove idle registry entries"
        );
        assert!(
            slot_lock_registry_capacity() <= baseline_capacity,
            "released handles must not retain registry capacity above its baseline"
        );
    }

    /// An eviction pass for one key must not delete another key while that
    /// key is inside its slot-locked mutation span. The active thread models
    /// the interval in which `ensure_clone` is blocked in `git fetch`; before
    /// eviction consulted candidate locks, the count cap deleted `active`
    /// despite its guard and the operation resumed over a missing slot.
    #[test]
    fn eviction_defers_a_candidate_with_an_active_slot_mutation() {
        let _guard = ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS", "1");
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", "1000000000");

        let root = dir.path();
        let active_key = "aaaaaaaaaaaaaaaa";
        let active = make_owned_entry(root, active_key, true);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let keep = make_owned_entry(root, "bbbbbbbbbbbbbbbb", true);

        let active_lock = slot_lock(active_key);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let active_for_thread = active.clone();
        let handle = std::thread::spawn(move || {
            let _active_guard = active_lock.lock().expect("lock active slot");
            started_tx.send(()).expect("signal active mutation");
            release_rx.recv().expect("release active mutation");
            assert!(
                active_for_thread.exists(),
                "an active slot must still exist when its mutation resumes"
            );
            std::fs::write(active_for_thread.join("mutation-complete"), b"")
                .expect("complete active mutation");
        });

        started_rx.recv().expect("wait for active mutation");
        evict_lru(root, &keep).expect("evict around active slot");
        assert!(active.exists(), "eviction must defer the active candidate");
        release_tx.send(()).expect("release active mutation");
        handle.join().expect("active mutation thread");
        assert!(active.join("mutation-complete").exists());

        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS");
        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES");
    }

    /// The concrete regression issue #805 describes: before `slot_lock`,
    /// concurrent `ensure_clone` calls for the same never-before-cached URL
    /// could both observe an absent slot and both proceed to
    /// `install_fresh_clone`, racing `std::fs::rename` onto the same
    /// `<root>/<cache_key>/` path -- the loser's rename fails because the
    /// winner already populated a non-empty directory there. With same-key
    /// mutation serialized, the loser instead waits, observes the slot the
    /// winner installed, and takes the existing-slot (`fetch`) path -- every
    /// concurrent call succeeds and resolves to the same slot.
    #[test]
    fn concurrent_ensure_clone_on_same_key_never_races_the_slot() {
        let _guard = ENV_MUTEX.blocking_lock();
        let scratch = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

        let origin_dir = tempfile::tempdir().expect("tempdir");
        init_origin_with_one_commit(origin_dir.path());
        let canonical = origin_dir.path().to_str().unwrap().to_string();

        const CONCURRENCY: usize = 6;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(CONCURRENCY));
        let handles: Vec<_> = (0..CONCURRENCY)
            .map(|_| {
                let canonical = canonical.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    ensure_clone(&canonical)
                })
            })
            .collect();

        let results: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().expect("ensure_clone thread panicked"))
            .collect();

        for result in &results {
            assert!(
                result.is_ok(),
                "concurrent ensure_clone calls on the same key must never race the slot: {result:?}"
            );
        }
        let first = results[0].as_ref().unwrap();
        for result in &results[1..] {
            assert_eq!(
                result.as_ref().unwrap(),
                first,
                "every concurrent call must resolve to the same cache slot"
            );
        }

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    }
}
