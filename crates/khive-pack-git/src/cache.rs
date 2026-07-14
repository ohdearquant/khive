//! Scratch-clone cache for `git.digest`'s remote-URL mode (ADR-088
//! Amendment 1). Clones/fetches into
//! `~/.khive/scratch/git-digest/<cache_key>/`, keyed by canonical URL
//! (`crate::source::cache_key`). An LRU cap (env-var configured:
//! `KHIVE_GIT_DIGEST_CACHE_MAX_REPOS`, `KHIVE_GIT_DIGEST_CACHE_MAX_BYTES`,
//! `KHIVE_GIT_DIGEST_CLONE_MAX_BYTES`, `KHIVE_GIT_DIGEST_SCRATCH_ROOT`)
//! evicts least-recently-used clones once the cache exceeds its repo-count
//! or total-byte limit; a per-clone size cap rejects an oversized
//! clone/fetch before it enters the addressable cache slot. See
//! crates/khive-pack-git/docs/cache.md for the full design rationale
//! (ownership-proof eviction, staging-then-move installation, per-clone cap
//! enforcement).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use uuid::Uuid;

use crate::source::cache_key;

pub const DEFAULT_MAX_REPOS: usize = 5;
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const DEFAULT_CLONE_MAX_BYTES: u64 = 1024 * 1024 * 1024;

const MARKER_FILE: &str = ".khive-last-used";

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
    /// crates/khive-pack-git/docs/cache.md#cacheerrorunsafetoreplace.
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
/// eviction pass). See crates/khive-pack-git/docs/cache.md#ensure_clone for
/// the staging-then-move and ownership-guard rationale.
pub fn ensure_clone(canonical_url: &str) -> Result<PathBuf, CacheError> {
    let root = scratch_root();
    std::fs::create_dir_all(&root)?;
    let key = cache_key(canonical_url);
    let repo_dir = root.join(&key);
    let cap = clone_max_bytes();

    if repo_dir.join(".git").exists() {
        if !is_owned_entry(&repo_dir) {
            return Err(CacheError::UnsafeToReplace(repo_dir));
        }
        fetch(&repo_dir)?;
        // `repo_dir` was just fetched into and its ownership already
        // confirmed above; it vanishing here is a real problem (e.g. a
        // racing, non-serialized `ensure_clone`/`reclone` on the same key --
        // see `refetch_clone`'s doc comment), not a maybe-absent slot, so
        // propagate rather than swallow.
        let size = dir_size(&repo_dir)?;
        if size > cap {
            remove_owned_entry(&root, &repo_dir)?;
            return Err(CacheError::CloneTooLarge { bytes: size, cap });
        }
        touch(&repo_dir)?;
    } else {
        install_fresh_clone(canonical_url, &root, &repo_dir, cap)?;
    }

    evict_lru(&root, &repo_dir)?;
    Ok(repo_dir)
}

/// Re-fetch a corrupt-but-present cache slot with `git fetch --refetch`
/// (issue #765), re-checking ownership immediately before fetching. See
/// crates/khive-pack-git/docs/cache.md#refetch_clone.
pub(crate) fn refetch_clone(canonical_url: &str) -> Result<PathBuf, CacheError> {
    let root = scratch_root();
    let key = cache_key(canonical_url);
    let repo_dir = root.join(&key);
    if !repo_dir.join(".git").exists() {
        return Err(CacheError::Git(format!(
            "refetch requested for {canonical_url:?} but no cache slot exists at {}",
            repo_dir.display()
        )));
    }
    // Re-check ownership immediately before mutating the slot (issue #765
    // follow-up PR #788) — see crates/khive-pack-git/docs/cache.md#refetch_clone.
    if !is_owned_entry(&repo_dir) {
        return Err(CacheError::UnsafeToReplace(repo_dir));
    }

    fetch_refetch(&repo_dir)?;

    let cap = clone_max_bytes();
    let size = dir_size(&repo_dir)?;
    if size > cap {
        // Ownership-guarded removal, not a raw `remove_dir_all` — see
        // crates/khive-pack-git/docs/cache.md#refetch_clone.
        remove_owned_entry(&root, &repo_dir)?;
        return Err(CacheError::CloneTooLarge { bytes: size, cap });
    }

    touch(&repo_dir)?;
    evict_lru(&root, &repo_dir)?;
    Ok(repo_dir)
}

/// Evict an owned cache slot (if present) and install a fresh clone in its
/// place (issue #765's fallback when a refetch cannot repair the slot). See
/// crates/khive-pack-git/docs/cache.md#reclone.
pub(crate) fn reclone(canonical_url: &str) -> Result<PathBuf, CacheError> {
    let root = scratch_root();
    std::fs::create_dir_all(&root)?;
    let key = cache_key(canonical_url);
    let repo_dir = root.join(&key);
    let cap = clone_max_bytes();

    remove_owned_entry(&root, &repo_dir)?;
    install_fresh_clone(canonical_url, &root, &repo_dir, cap)?;

    evict_lru(&root, &repo_dir)?;
    Ok(repo_dir)
}

/// Shared staging-clone-then-move path for both a first-time `ensure_clone`
/// and a `reclone` repair. See
/// crates/khive-pack-git/docs/cache.md#install_fresh_clone.
fn install_fresh_clone(
    canonical_url: &str,
    root: &Path,
    repo_dir: &Path,
    cap: u64,
) -> Result<(), CacheError> {
    let staging_dir = root.join(format!(".staging-{}", Uuid::new_v4()));
    clone(canonical_url, &staging_dir).inspect_err(|_| {
        // `git clone` can create and partially populate the destination
        // before failing (network drop, auth failure, bad ref) -- clean
        // it up so a run of failures doesn't leave `.staging-*` litter
        // under the scratch root. `evict_lru` deliberately never touches
        // non-owned names (`is_owned_entry`), so nothing else would ever
        // reclaim this on its own.
        let _ = std::fs::remove_dir_all(&staging_dir);
    })?;
    let size = dir_size(&staging_dir).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&staging_dir);
    })?;
    if size > cap {
        let _ = std::fs::remove_dir_all(&staging_dir);
        return Err(CacheError::CloneTooLarge { bytes: size, cap });
    }
    touch(&staging_dir).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&staging_dir);
    })?;
    std::fs::rename(&staging_dir, repo_dir).map_err(|e| {
        let _ = std::fs::remove_dir_all(&staging_dir);
        CacheError::Io(e)
    })?;
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
    remove_dir_all_retrying(repo_dir).map_err(CacheError::Io)?;
    Ok(())
}

/// Retries `remove_dir_all` a few times before giving up — see
/// crates/khive-pack-git/docs/cache.md#remove_dir_all_retrying.
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
/// crates/khive-pack-git/docs/cache.md#clone-git-subprocess-maintenanceautofalse.
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
        .arg(url)
        .arg(dest)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .map_err(|e| CacheError::Git(format!("spawning git clone: {e}")))?;
    if !status.success() {
        return Err(CacheError::Git(format!(
            "git clone {url} failed (exit {status})"
        )));
    }
    Ok(())
}

fn fetch(repo: &Path) -> Result<(), CacheError> {
    let status = Command::new("git")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("-c")
        .arg("maintenance.auto=false")
        .arg("-C")
        .arg(repo)
        .arg("fetch")
        .arg("--prune")
        .env("GIT_TERMINAL_PROMPT", "0")
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
fn fetch_refetch(repo: &Path) -> Result<(), CacheError> {
    let status = Command::new("git")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("-c")
        .arg("maintenance.auto=false")
        .arg("-C")
        .arg(repo)
        .arg("fetch")
        .arg("--refetch")
        .arg("origin")
        .env("GIT_TERMINAL_PROMPT", "0")
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

fn touch(repo_dir: &Path) -> Result<(), CacheError> {
    let marker = repo_dir.join(MARKER_FILE);
    std::fs::write(&marker, b"").map_err(|e| io_err("touch: write marker", &marker, e))?;
    Ok(())
}

/// Recursive directory size, following no symlinks. Tolerant of a
/// *descendant* disappearing mid-walk (contributes 0 bytes); the walk
/// **root** itself vanishing is NOT tolerated and surfaces as
/// `CacheError::Io(NotFound)`. See
/// crates/khive-pack-git/docs/cache.md#dir_size.
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

/// Whether `path` is a directory `ensure_clone` could plausibly have
/// created: a 16-lowercase-hex `cache_key`-shaped real directory (not a
/// symlink) containing both a `.git` entry and the `.khive-last-used`
/// marker. See crates/khive-pack-git/docs/cache.md#is_owned_entry.
fn is_owned_entry(path: &Path) -> bool {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };
    if name.len() != 16
        || !name
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
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
/// (skipped). See crates/khive-pack-git/docs/cache.md#evict_lru.
fn evict_lru(root: &Path, keep: &Path) -> Result<(), CacheError> {
    let mut entries: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
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
        if !p.is_dir() || p == keep || !is_owned_entry(&p) {
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
        entries.push((p, mtime, size));
    }
    entries.sort_by_key(|(_, mtime, _)| *mtime);

    let keep_size = dir_size(keep)?;
    let mut total: u64 = entries.iter().map(|(_, _, s)| s).sum::<u64>() + keep_size;
    let mut count = entries.len() + 1;
    let cap_repos = max_repos();
    let cap_bytes = max_total_bytes();

    for (path, _, size) in entries {
        if count <= cap_repos && total <= cap_bytes {
            break;
        }
        let _ = std::fs::remove_dir_all(&path);
        count -= 1;
        total = total.saturating_sub(size);
    }
    Ok(())
}

/// Serializes tests that touch process-global env vars (`scratch_root()`
/// reads them). See crates/khive-pack-git/docs/cache.md#env_mutex.
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

    /// A `git clone` failure must not leave a `.staging-<uuid>` dir behind.
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

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read scratch root")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".staging-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a failed clone must not leave .staging-* directories behind: {leftovers:?}"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
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

        evict_lru(&root, &kept).expect("evict");

        assert!(
            no_marker.exists(),
            "an owned-looking directory without the marker must survive eviction"
        );

        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS");
        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES");
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
    /// crates/khive-pack-git/docs/cache.md#test-module-notes.
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
    /// crates/khive-pack-git/docs/cache.md#test-module-notes.
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
    /// crates/khive-pack-git/docs/cache.md#test-module-notes.
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
    /// crates/khive-pack-git/docs/cache.md#test-module-notes.
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
    /// crates/khive-pack-git/docs/cache.md#test-module-notes.
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

    /// #765's fallback path — see
    /// crates/khive-pack-git/docs/cache.md#test-module-notes.
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
    /// crates/khive-pack-git/docs/cache.md#test-module-notes.
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
    /// crates/khive-pack-git/docs/cache.md#test-module-notes.
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
}
