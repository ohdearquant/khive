//! Scratch-clone cache for `git.digest`'s remote-URL mode (ADR-088
//! Amendment 1).
//!
//! Clones/fetches into `~/.khive/scratch/git-digest/<cache_key>/`, keyed by
//! canonical URL (`crate::source::cache_key`). An LRU cap evicts the
//! least-recently-used clone (by a `.khive-last-used` marker file's mtime,
//! touched on every successful `ensure_clone`) once the cache exceeds
//! `digest_cache_max_repos` entries or `digest_cache_max_bytes` total size --
//! eviction is safe because ingest cursors live in the database, not the
//! clone (ADR-088 Amendment 1 §Remote-URL mode). Eviction only ever removes
//! entries it can *prove* it owns (`is_owned_entry`: a 16-hex cache-key
//! directory name containing both a `.git` dir and the `.khive-last-used`
//! marker) -- a `KHIVE_GIT_DIGEST_SCRATCH_ROOT` override pointed at a broader
//! or pre-existing directory must never lose unrelated operator data.
//!
//! A per-clone size cap (`digest_cache_clone_max_bytes`) rejects a clone/
//! fetch that grows past its own budget *before* it ever enters the
//! addressable cache slot: `ensure_clone` clones/fetches into a staging
//! directory outside the cache root, measures it, and only moves it into
//! `<root>/<cache_key>/` when it is under the cap. A too-large clone is
//! deleted from staging and never touches `evict_lru`'s bookkeeping or the
//! cache slot. This guarantees the cap is enforced before the clone enters
//! the cache -- it does NOT bound the transient disk usage of the clone/
//! fetch child process itself while it runs in staging (`git` has no
//! reliable pre-flight or mid-transfer size check for a partial
//! `--filter=blob:none` clone); a single oversized `git clone` can still
//! transiently consume disk in the staging directory before this check
//! rejects and removes it.
//!
//! Config is env-var driven today (`KHIVE_GIT_DIGEST_CACHE_MAX_REPOS`,
//! `KHIVE_GIT_DIGEST_CACHE_MAX_BYTES`, `KHIVE_GIT_DIGEST_CLONE_MAX_BYTES`,
//! `KHIVE_GIT_DIGEST_SCRATCH_ROOT`) rather than a `[git]` TOML section --
//! see the implementation report for why.

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
    CloneTooLarge { bytes: u64, cap: u64 },
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
/// An existing cache slot is updated in place (`git fetch --prune`) and
/// re-measured against the per-clone cap -- a repo that grew past the cap
/// since it was last fetched is evicted from the cache slot on the spot. A
/// fresh clone is written into a private staging directory first
/// (`git clone --filter=blob:none`), measured there, and only *moved* into
/// the addressable `<root>/<cache_key>/` slot once it is under the cap --
/// an oversized clone never enters the cache slot, never participates in
/// `evict_lru`'s accounting, and is removed from staging immediately.
///
/// Runs LRU eviction over the rest of the cache after a successful
/// clone/fetch (this clone is exempt from its own eviction pass).
pub fn ensure_clone(canonical_url: &str) -> Result<PathBuf, CacheError> {
    let root = scratch_root();
    std::fs::create_dir_all(&root)?;
    let key = cache_key(canonical_url);
    let repo_dir = root.join(&key);
    let cap = clone_max_bytes();

    if repo_dir.join(".git").exists() {
        fetch(&repo_dir)?;
        let size = dir_size(&repo_dir)?;
        if size > cap {
            let _ = std::fs::remove_dir_all(&repo_dir);
            return Err(CacheError::CloneTooLarge { bytes: size, cap });
        }
    } else {
        let staging_dir = root.join(format!(".staging-{}", Uuid::new_v4()));
        clone(canonical_url, &staging_dir)?;
        let size = dir_size(&staging_dir).inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&staging_dir);
        })?;
        if size > cap {
            let _ = std::fs::remove_dir_all(&staging_dir);
            return Err(CacheError::CloneTooLarge { bytes: size, cap });
        }
        std::fs::rename(&staging_dir, &repo_dir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&staging_dir);
            CacheError::Io(e)
        })?;
    }

    touch(&repo_dir)?;
    evict_lru(&root, &repo_dir)?;
    Ok(repo_dir)
}

fn clone(url: &str, dest: &Path) -> Result<(), CacheError> {
    let status = Command::new("git")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
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

fn touch(repo_dir: &Path) -> Result<(), CacheError> {
    std::fs::write(repo_dir.join(MARKER_FILE), b"")?;
    Ok(())
}

/// Recursive directory size, following no symlinks (`symlink_metadata`
/// throughout, so a symlink itself is sized but never traversed -- clones
/// never legitimately contain symlinked directories pointing outside the
/// clone, and this avoids any possibility of a symlink loop).
fn dir_size(path: &Path) -> Result<u64, CacheError> {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let md = std::fs::symlink_metadata(&p)?;
        if md.is_dir() {
            for entry in std::fs::read_dir(&p)? {
                stack.push(entry?.path());
            }
        } else {
            total += md.len();
        }
    }
    Ok(total)
}

/// Whether `path` is a directory `ensure_clone` could plausibly have
/// created: a 16-lowercase-hex `cache_key`-shaped directory name (never a
/// UUID staging dir, never an arbitrary operator directory) containing both
/// a `.git` entry and the `.khive-last-used` marker written by `touch`.
/// Eviction (and any future scratch-root cleanup) must only ever remove
/// entries that pass this check -- a `KHIVE_GIT_DIGEST_SCRATCH_ROOT`
/// override pointed at a broader or pre-existing directory must never lose
/// unrelated data sitting next to the cache slots.
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
    path.join(".git").exists() && path.join(MARKER_FILE).exists()
}

/// Evict least-recently-used clones under `root` (by `.khive-last-used`
/// mtime) until both the repo-count cap and the total-byte cap are
/// satisfied. `keep` (the clone `ensure_clone` just touched) is never
/// evicted. Only removes paths that are direct children of `root` AND pass
/// `is_owned_entry` -- eviction never touches user-owned or non-cache paths.
fn evict_lru(root: &Path, keep: &Path) -> Result<(), CacheError> {
    let mut entries: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let p = entry.path();
        if !p.is_dir() || p == keep || !is_owned_entry(&p) {
            continue;
        }
        let mtime = std::fs::metadata(p.join(MARKER_FILE))
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let size = dir_size(&p)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;

    /// `scratch_root()` reads process-global env vars; serialize tests that
    /// touch it.
    static ENV_MUTEX: LazyLock<std::sync::Mutex<()>> = LazyLock::new(|| std::sync::Mutex::new(()));

    /// Build a directory shaped exactly like a real `ensure_clone` cache
    /// slot: a 16-lowercase-hex name (a real `cache_key` output) containing
    /// a `.git` dir and (optionally) the `.khive-last-used` marker.
    fn make_owned_entry(root: &Path, key: &str, with_marker: bool) -> PathBuf {
        assert_eq!(key.len(), 16, "test cache keys must be 16 hex chars");
        let p = root.join(key);
        std::fs::create_dir_all(p.join(".git")).unwrap();
        if with_marker {
            std::fs::write(p.join(MARKER_FILE), b"").unwrap();
        }
        p
    }

    #[test]
    fn evict_lru_removes_oldest_past_repo_cap() {
        let _guard = ENV_MUTEX.lock().unwrap();
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
        let _guard = ENV_MUTEX.lock().unwrap();
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
        let _guard = ENV_MUTEX.lock().unwrap();
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
        let _guard = ENV_MUTEX.lock().unwrap();
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
}
