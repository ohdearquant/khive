//! Scratch-clone cache for `git.digest`'s remote-URL mode (ADR-088
//! Amendment 1).
//!
//! Clones/fetches into `~/.khive/scratch/git-digest/<cache_key>/`, keyed by
//! canonical URL (`crate::source::cache_key`). An LRU cap evicts the
//! least-recently-used clone (by a `.khive-last-used` marker file's mtime,
//! touched on every successful `ensure_clone`) once the cache exceeds
//! `digest_cache_max_repos` entries or `digest_cache_max_bytes` total size --
//! eviction is safe because ingest cursors live in the database, not the
//! clone (ADR-088 Amendment 1 §Remote-URL mode). A per-clone size cap
//! (`digest_cache_clone_max_bytes`) aborts an individual clone/fetch that
//! grows past its own budget: `git` has no reliable pre-flight size check for
//! a partial (`--filter=blob:none`) clone, so the cap is enforced by
//! clone-then-measure-then-clean-up-on-violation rather than a pre-check.
//!
//! Config is env-var driven today (`KHIVE_GIT_DIGEST_CACHE_MAX_REPOS`,
//! `KHIVE_GIT_DIGEST_CACHE_MAX_BYTES`, `KHIVE_GIT_DIGEST_CLONE_MAX_BYTES`,
//! `KHIVE_GIT_DIGEST_SCRATCH_ROOT`) rather than a `[git]` TOML section --
//! see the implementation report for why.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

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
/// the repo's local path. No existing clone -> `git clone --filter=blob:none`.
/// Existing clone -> `git fetch --prune`. Enforces the per-clone size cap
/// after the clone/fetch completes, and runs LRU eviction over the rest of
/// the cache (this clone is exempt from its own eviction pass).
pub fn ensure_clone(canonical_url: &str) -> Result<PathBuf, CacheError> {
    let root = scratch_root();
    std::fs::create_dir_all(&root)?;
    let key = cache_key(canonical_url);
    let repo_dir = root.join(&key);

    if repo_dir.join(".git").exists() {
        fetch(&repo_dir)?;
    } else {
        clone(canonical_url, &repo_dir)?;
    }

    let size = dir_size(&repo_dir)?;
    let cap = clone_max_bytes();
    if size > cap {
        let _ = std::fs::remove_dir_all(&repo_dir);
        return Err(CacheError::CloneTooLarge { bytes: size, cap });
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

/// Evict least-recently-used clones under `root` (by `.khive-last-used`
/// mtime) until both the repo-count cap and the total-byte cap are
/// satisfied. `keep` (the clone `ensure_clone` just touched) is never
/// evicted. Only removes paths that are direct children of `root` --
/// eviction never touches user-owned paths.
fn evict_lru(root: &Path, keep: &Path) -> Result<(), CacheError> {
    let mut entries: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let p = entry.path();
        if !p.is_dir() || p == keep {
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

    #[test]
    fn evict_lru_removes_oldest_past_repo_cap() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", dir.path());
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS", "1");
        std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", "1000000000");

        let root = dir.path();
        let old = root.join("old-repo");
        let new = root.join("new-repo");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join(MARKER_FILE), b"").unwrap();
        // Ensure a real mtime gap.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(new.join(MARKER_FILE), b"").unwrap();

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
        let kept = root.join("kept");
        std::fs::create_dir_all(&kept).unwrap();
        std::fs::write(kept.join(MARKER_FILE), b"").unwrap();

        evict_lru(&root, &kept).expect("evict");
        assert!(kept.exists());

        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS");
        std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES");
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
