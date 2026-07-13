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
    CloneTooLarge {
        bytes: u64,
        cap: u64,
    },
    /// A repair operation (refetch/reclone) would have to touch a path that
    /// does not prove itself an owned cache slot (`is_owned_entry`) or is
    /// not a direct child of the scratch root — refused rather than risking
    /// deletion of unrelated operator data under an overridden
    /// `KHIVE_GIT_DIGEST_SCRATCH_ROOT`.
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
/// An existing path at the cache-key slot is only ever treated as a fetchable
/// cache slot when it already passes `is_owned_entry` -- a `.git` directory
/// sitting at that path without the `.khive-last-used` marker (a foreign
/// directory that happens to collide with the cache key, or a directory a
/// crashed prior run left in a pre-`touch` state) is refused with
/// `CacheError::UnsafeToReplace` rather than fetched into or adopted (issue
/// #765). A fresh clone is written into a private
/// staging directory first (`git clone --filter=blob:none`), measured there,
/// marked with `.khive-last-used` there, and only *moved* into the
/// addressable `<root>/<cache_key>/` slot once it is under the cap and
/// already carries its ownership marker -- an oversized clone never enters
/// the cache slot, never participates in `evict_lru`'s accounting, and is
/// removed from staging immediately; a process interruption between the
/// clone and the rename can never leave a live, markerless slot behind.
///
/// A repo that grew past the per-clone cap since it was last fetched is
/// evicted from the cache slot on the spot, through the same ownership-
/// guarded `remove_owned_entry` every other repair path uses, propagating
/// any cleanup/ownership failure instead of discarding it.
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
/// (issue #765): downloads a complete fresh filtered packfile rather than
/// trusting the existing (possibly promisor-incomplete) object store,
/// repairing a partial/pruned clone in place. Only ever operates on an
/// existing slot -- callers repair a slot only after a prior `ensure_clone`
/// already produced one. Re-checks `is_owned_entry` immediately before
/// fetching (issue #765 follow-up PR #788): the
/// gap between `ensure_clone`'s own ownership check and this repair running
/// -- project resolution and GitHub ingestion happen in between -- is wide
/// enough for the slot to go markerless or be replaced, so this function
/// cannot rely on the caller having checked recently.
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
    // follow-up PR #788): the caller's own
    // ownership check (`ensure_clone`, much earlier in `handle_digest`)
    // happens before project resolution and potentially lengthy GitHub
    // ingestion, so the slot can go markerless -- or be replaced by a
    // foreign directory colliding with the cache key -- in that interval.
    // Without this re-check, `fetch_refetch` below would mutate whatever
    // sits at `repo_dir` and `touch` would mark it owned, making it eligible
    // for later deletion. There is no same-key serialization for cache
    // mutation in this crate today (a concurrent `ensure_clone`/`reclone`
    // racing this same slot is not otherwise excluded) -- this re-check
    // narrows the adoption bug but does not close a true concurrent-writer
    // race.
    if !is_owned_entry(&repo_dir) {
        return Err(CacheError::UnsafeToReplace(repo_dir));
    }

    fetch_refetch(&repo_dir)?;

    let cap = clone_max_bytes();
    // Same reasoning as `ensure_clone`: `repo_dir`'s ownership was just
    // re-checked and it was just fetched into, so a vanish here is a real
    // problem to surface, not a maybe-absent slot to size as `0`.
    let size = dir_size(&repo_dir)?;
    if size > cap {
        // Route through the same ownership-guarded removal `reclone` uses
        // (issue #765 remediation) rather than a raw
        // `remove_dir_all` -- a repair primitive must never delete a path
        // that doesn't prove itself an owned cache slot, even on the cap-
        // exceeded cleanup path. Propagate a cleanup/ownership failure
        // instead of discarding it: a refused or
        // failed removal must surface as its own error, not be silently
        // swallowed behind `CloneTooLarge`.
        remove_owned_entry(&root, &repo_dir)?;
        return Err(CacheError::CloneTooLarge { bytes: size, cap });
    }

    touch(&repo_dir)?;
    evict_lru(&root, &repo_dir)?;
    Ok(repo_dir)
}

/// Evict an owned cache slot (if present) and install a fresh clone in its
/// place (issue #765's fallback when a refetch cannot repair the slot).
/// Refuses via `CacheError::UnsafeToReplace` when the existing path does not
/// prove itself an owned cache slot -- the same ownership guard `evict_lru`
/// uses, so a `KHIVE_GIT_DIGEST_SCRATCH_ROOT` override pointed at a broader
/// or pre-existing directory can never lose unrelated operator data here
/// either.
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
/// and a `reclone` repair: clones into a private staging directory outside
/// the cache root, measures it against the per-clone cap, writes the
/// `.khive-last-used` ownership marker into the staging directory itself,
/// and only then moves it into the addressable `<root>/<cache_key>/` slot --
/// an oversized clone never enters the cache slot, and because the marker is
/// written before the atomic rename, a process interruption between clone
/// and rename can never leave a live, markerless slot at the cache-key path
/// (issue #765).
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
/// `is_owned_entry` -- refuses (`CacheError::UnsafeToReplace`) rather than
/// deleting anything else, including a not-yet-existing or foreign-shaped
/// path. A slot that does not currently exist is not an error: there is
/// simply nothing to remove before installing a fresh clone.
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

/// `std::fs::remove_dir_all` on a large git working tree can transiently
/// fail with "directory not empty" when something else briefly touches the
/// tree mid-removal (e.g. a filesystem indexer) -- retry a few times before
/// giving up, rather than letting a one-off transient race abort a repair
/// that would otherwise succeed.
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

/// `-c maintenance.auto=false` on every clone/fetch into a cache slot, as
/// defensive hardening. `git fetch` runs auto-maintenance after it
/// finishes when `maintenance.auto` (default true) is set, and since git
/// 2.47 that maintenance runs as a *detached background child*
/// (`git maintenance run --auto --detach`) that can outlive the foreground
/// command; on 2.46 and earlier it ran synchronously. The spawn is
/// trace2-proven in both directions on the `fetch --refetch` path
/// (`GIT_TRACE2_EVENT`, git 2.49: with default config the child forks; with
/// `maintenance.auto=false` it does not). The same trace showed `clone`
/// spawning no maintenance child; the flag is applied to the clone builder
/// too purely as harmless defensive configuration, with no trace evidence
/// claimed for that path. When one of the detached child's tasks fires it
/// mutates the slot's `.git` tree (commit-graph writes, pack maintenance,
/// lock files) concurrently with any `dir_size`/`evict_lru` walk of the
/// same slot. Whether such a task actually fired in issue #842's historical
/// macOS ENOENT failures is not proven -- in small repos the child
/// typically finds no task to run and exits quickly -- so the load-bearing
/// fix for that flake family is the descendant-vanish tolerance in
/// `dir_size`; this flag removes the one background mutator git itself can
/// fork into our cache slots. `gc.auto=0` alone does **not** suppress the
/// child (trace2-verified); it is kept alongside because it disables
/// `git gc --auto`'s separate opportunistic-gc check, harmless to also turn
/// off here.
///
/// This does not mean a cache slot is naturally garbage-collected some other
/// way instead: no cache-slot repo is ever gc'd or maintenance'd by us.
/// Growth is bounded by wholesale eviction, not in-place compaction --
/// `ensure_clone`/`refetch_clone` delete a slot outright
/// (`remove_owned_entry`) the moment it measures over
/// `digest_cache_clone_max_bytes` after a fetch, and `evict_lru` deletes
/// whole least-recently-used slot directories once the cache-wide
/// `digest_cache_max_repos`/`digest_cache_max_bytes` caps are exceeded. A
/// slot can be fetched into repeatedly, but it can never accumulate objects
/// past its own size cap without being deleted and re-cloned fresh, so there
/// is nothing for git's own gc/maintenance to usefully do in a cache slot.
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
/// existing (possibly promisor-incomplete) object store -- the documented
/// fix for a partial clone that has dropped objects it should still have.
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

/// Wrap an I/O error with the operation and path it happened on -- a bare
/// `CacheError::Io(e)` at these call sites used to surface as an opaque
/// "No such file or directory" with no way to tell which of the many paths
/// `dir_size`/`touch`/`evict_lru` touch actually disappeared.
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

/// Recursive directory size, following no symlinks (`symlink_metadata`
/// throughout, so a symlink itself is sized but never traversed -- clones
/// never legitimately contain symlinked directories pointing outside the
/// clone, and this avoids any possibility of a symlink loop).
///
/// Tolerant of a *descendant* disappearing mid-walk (a vanished entry
/// beneath an existing root contributes 0 bytes rather than aborting the
/// whole size computation): a cache slot's `.git` tree can legitimately be
/// mutated by something outside this function's control while it walks it
/// -- a concurrent `evict_lru`/`ensure_clone` repair on the same slot, or a
/// background `git maintenance` child from before `maintenance.auto=false`
/// applied to every command this crate issues. This accounting is
/// inherently a snapshot of a possibly-changing tree (ADR-088 Amendment 1:
/// eviction is safe because ingest cursors live in the database, not the
/// clone), so "a thing under the root I was about to size is already gone"
/// is not an error here.
///
/// The walk **root** itself vanishing is different and is NOT tolerated --
/// it surfaces as `CacheError::Io(NotFound)` rather than silently sizing to
/// `0`. A caller that genuinely expects the root it's sizing to sometimes be
/// absent (rather than an existing root racing a mid-walk mutation) must
/// check for that error explicitly and decide its own semantics at that call
/// site (`evict_lru` does this for a listed entry that a concurrent repair
/// deleted between `read_dir` and this call -- see there); `dir_size` itself
/// never launders a missing root into a bare `0`, which previously let
/// `evict_lru` report success with a missing keep slot or count a phantom
/// candidate and evict a valid one unnecessarily.
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
/// created: a 16-lowercase-hex `cache_key`-shaped directory name (never a
/// UUID staging dir, never an arbitrary operator directory), itself a real
/// directory rather than a symlink (a symlink placed at the cache-key path
/// pointing at an unrelated owned-looking or foreign directory must never be
/// treated as an owned slot), containing both a `.git` entry and the
/// `.khive-last-used` marker written by `touch`. Eviction (and any future
/// scratch-root cleanup) must only ever remove entries that pass this check
/// -- a `KHIVE_GIT_DIGEST_SCRATCH_ROOT` override pointed at a broader or
/// pre-existing directory must never lose unrelated data sitting next to the
/// cache slots.
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

/// Evict least-recently-used clones under `root` (by `.khive-last-used`
/// mtime) until both the repo-count cap and the total-byte cap are
/// satisfied. `keep` (the clone `ensure_clone` just touched) is never
/// evicted. Only removes paths that are direct children of `root` AND pass
/// `is_owned_entry` -- eviction never touches user-owned or non-cache paths.
///
/// `keep`'s own `dir_size` (below) is deliberately NOT tolerant of `keep`
/// vanishing: every caller touches (or freshly installs) `keep` immediately
/// before calling `evict_lru` in the same synchronous call chain, so `keep`
/// disappearing out from under this call is not an expected repair race --
/// it is either a genuine bug or an external actor deleting our slot, and
/// silently sizing it to `0` would let eviction report success while the
/// slot the caller asked to keep is actually gone. A listed *candidate*
/// entry (below) is different -- another `evict_lru`/`ensure_clone`
/// repairing the same root can legitimately delete it between the
/// `read_dir` listing above and the `dir_size` call below, so that vanish is
/// tolerated by skipping the entry rather than aborting the whole pass.
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

/// `scratch_root()` reads process-global env vars; serialize any in-crate
/// test (in this module or elsewhere, e.g. `recovery_tests.rs`) that touches
/// it, so the whole `cargo test` binary's parallel test threads never race
/// on `KHIVE_GIT_DIGEST_SCRATCH_ROOT`/cache-cap env vars/`PATH`. A
/// `tokio::sync::Mutex` rather than `std::sync::Mutex` so async tests can
/// hold the guard across `.await` points (`blocking_lock()` for this
/// module's plain sync `#[test]`s).
#[cfg(test)]
pub(crate) static ENV_MUTEX: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

#[cfg(test)]
mod tests {
    use super::*;

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

    /// ADR-088 Amendment 1: a `git clone` failure (bad
    /// source, no network needed -- a nonexistent local path fails
    /// immediately) must not leave a `.staging-<uuid>` directory behind.
    /// `evict_lru` deliberately never touches non-owned names, so a leaked
    /// staging dir would otherwise accumulate forever across repeated
    /// failures.
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

    /// PR #847: the walk root vanishing must surface as an
    /// error, never a laundered `Ok(0)` -- distinct from a descendant
    /// vanishing beneath a still-existing root (see the Barrier tests
    /// below). A root that was never there to begin with is the simplest,
    /// fully deterministic instance of "the root itself is missing".
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

    /// `evict_lru`'s `keep` argument: every caller (`ensure_clone`,
    /// `refetch_clone`, `reclone`) has just touched or freshly installed
    /// `keep` immediately before calling `evict_lru`, so `keep` vanishing is
    /// a real problem to surface, not a maybe-absent slot -- `evict_lru`
    /// must propagate `dir_size(keep)`'s error rather than treat it as an
    /// empty, evictable-looking slot.
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

    /// Issue #842's macOS ENOENT flake family: `dir_size` walks a tree that
    /// can legitimately be mutated out from under it (git's own detached
    /// background maintenance child, or a racing `evict_lru`/`ensure_clone`
    /// repair on the same
    /// slot) -- a subdirectory disappearing between `read_dir` listing it and
    /// this walk descending into it must shrink the total, not abort the
    /// whole computation with `CacheError::Io(NotFound)`.
    ///
    /// This is a genuine cross-thread filesystem race, not a fully
    /// deterministic single-shot repro: a `std::sync::Barrier` releases both
    /// threads at the same instant, a wide fan of sibling subdirectories
    /// gives the walk many entries to still be processing when the deleter
    /// runs, and the whole race is repeated many times so the window is
    /// almost certain to be hit at least once across the loop. Pre-fix (see
    /// the sabotage note on `dir_size` above), this reliably reproduces
    /// `CacheError::Io` within a handful of iterations on this machine; it is
    /// not a `sleep`-based synchronization, so it is not always the exact
    /// same interleaving twice, but the failure is real and observable, not
    /// theoretical.
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

    /// Companion to the test above, pinning the other half of the
    /// contract (PR #847): when the vanishing path is the walk
    /// **root** itself -- not a descendant beneath a still-existing root --
    /// `dir_size` must surface an error rather than tolerate it. Same
    /// barrier-race harness, but `root` is left empty (an empty-directory
    /// removal is a single `rmdir` syscall, the same order of cost as the
    /// `symlink_metadata`/`read_dir` calls `dir_size` opens with -- a
    /// populated root, by contrast, has its own directory entry removed
    /// *last* by `remove_dir_all` after every child, well after the
    /// walker's first two syscalls would already have completed, which
    /// would make the root-vanish race effectively unreachable). Pre-fix,
    /// `dir_size` treated a disappearing root exactly like a disappearing
    /// descendant and returned `Ok(0)`, which could let `evict_lru` report
    /// success over a missing `keep` slot or count a phantom `0`-byte
    /// candidate.
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

    /// A real local repo usable as an `ensure_clone`/`refetch_clone`/`reclone`
    /// `canonical_url` (git accepts a plain filesystem path as a clone/fetch
    /// source, same as the existing `ensure_clone_cleans_up_staging_dir_*`
    /// test does for a failure case).
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

    /// The primary #765 acceptance path: a slot already exists (via
    /// `ensure_clone`); `refetch_clone` must pull history the slot doesn't
    /// have yet (standing in for genuinely corrupt/incomplete objects, which
    /// `git fetch --refetch` repairs the same way -- by re-obtaining a
    /// complete fresh packfile from the remote).
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

    /// Remediation (issue #765): `refetch_clone`'s over-cap cleanup must go through
    /// the same ownership guard `reclone` uses, not a raw `remove_dir_all`,
    /// AND must propagate that guard's failure rather than discarding it --
    /// a slot that has lost its `.khive-last-used` marker (simulating a
    /// directory the guard cannot prove it owns) survives over-cap cleanup,
    /// and the caller sees the ownership failure (`UnsafeToReplace`) that
    /// actually occurred, not a laundered `CloneTooLarge`. Since a later
    /// fix added a pre-fetch ownership re-check, this markerless
    /// slot is now refused before `fetch_refetch` even runs (see
    /// `refetch_clone_refuses_a_markerless_slot_under_the_cap` below) rather
    /// than at the over-cap cleanup step this test originally targeted --
    /// the assertions still hold (`UnsafeToReplace`, slot survives), so this
    /// remains a valid regression guard for the cleanup path once a slot
    /// somehow reaches it un-owned.
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

    /// Remediation (issue #765 follow-up PR #788):
    /// `refetch_clone` must refuse a markerless slot *before* ever calling
    /// `fetch_refetch`, not only on the over-cap cleanup branch the previous
    /// test exercises. Under the default (non-cap-exceeded) cap, a
    /// pre-fetch ownership check is the only thing standing between a
    /// markerless slot and a real fetch: the origin is given fresh history
    /// so a fetch that ran despite the missing marker would be directly
    /// observable via a moved `HEAD`.
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

    /// #765's fallback path: a refetch that cannot repair the slot (here,
    /// simulated by pointing the existing slot's `origin` remote at a
    /// nonexistent path so `git fetch --refetch` itself fails) is followed by
    /// `reclone`, which ignores the broken clone entirely and clones fresh
    /// from the still-good `canonical_url`.
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

    /// Ownership guard (ADR-088 Amendment 1 / PR #761): `reclone` must never
    /// delete a directory that doesn't prove itself an owned cache slot, even
    /// though its path is exactly where the cache key says the slot should
    /// be -- a `KHIVE_GIT_DIGEST_SCRATCH_ROOT` override pointed at a broader
    /// directory must never lose unrelated operator data to a repair.
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

    /// When no slot exists at all yet, `reclone` has nothing to remove and
    /// simply installs a fresh clone -- the same fallback a first-ever
    /// `ensure_clone` would have taken.
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

    /// Remediation (issue #765): `ensure_clone` must
    /// never adopt, fetch into, or touch a directory sitting at the
    /// cache-key path that does not already prove itself owned via
    /// `is_owned_entry`. Here the directory is a genuine Git repository (so
    /// the pre-fix `repo_dir.join(".git").exists()` check alone would have
    /// accepted it) but is missing the `.khive-last-used` marker -- standing
    /// in for an operator's own repository that happens to land on the same
    /// cache-key path under an overridden `KHIVE_GIT_DIGEST_SCRATCH_ROOT`.
    /// The call must refuse with `UnsafeToReplace` and the sentinel operator
    /// data inside must survive completely untouched.
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

    /// Same guard, symlink variant: a symlink placed at the cache-key path
    /// pointing at an unrelated owned-looking directory must not be treated
    /// as an owned slot either -- `is_owned_entry` requires the cache-key
    /// path itself to be a real directory, not a symlink to one.
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
