use super::*;

#[test]
fn git_diagnostic_capture_is_bounded_drained_and_redacted() {
    let mut input = std::io::Cursor::new(vec![b'x'; MAX_GIT_DIAGNOSTIC_BYTES * 3]);
    let captured =
        capture_diagnostic(&mut input, &AtomicBool::new(false)).expect("capture diagnostic");
    assert_eq!(captured.len(), MAX_GIT_DIAGNOSTIC_BYTES);
    assert_eq!(input.position(), (MAX_GIT_DIAGNOSTIC_BYTES * 3) as u64);

    let sanitized = sanitize_diagnostic(
            "fatal: authentication failed for 'https://user:tok3n@example.com/repo?token=SECRET'\nAuthorization: Bearer PRIVATE\nCookie: session=SESSION\nfatal: https://user:partial\nerror: connection refused\u{001b}",
        );
    assert!(sanitized.contains("authentication failed"), "{sanitized}");
    assert!(sanitized.contains("connection refused"), "{sanitized}");
    for secret in [
        "user", "tok3n", "SECRET", "PRIVATE", "SESSION", "partial", "\u{001b}",
    ] {
        assert!(!sanitized.contains(secret), "{sanitized}");
    }
}

#[test]
fn git_diagnostic_capture_stops_without_descendant_eof() {
    struct OpenPipe;
    impl Read for OpenPipe {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::WouldBlock.into())
        }
    }
    assert!(capture_diagnostic(OpenPipe, &AtomicBool::new(true))
        .unwrap()
        .is_empty());
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::fd::OwnedFd;
        let (reader, mut retained_writer) = std::os::unix::net::UnixStream::pair().unwrap();
        retained_writer.write_all(b"fatal: unavailable").unwrap();
        let stderr = std::process::ChildStderr::from(OwnedFd::from(reader));
        let pipe = DiagnosticPipe::new(stderr).unwrap();
        assert_eq!(
            capture_diagnostic(pipe, &AtomicBool::new(true)).unwrap(),
            b"fatal: unavailable"
        );
        drop(retained_writer);
    }
}

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

#[cfg(windows)]
fn windows_fetchable_slot(base: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let upstream = base.join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    init_origin_with_one_commit(&upstream);
    let foreign = base.join("foreign");
    std::fs::create_dir_all(&foreign).unwrap();
    git(&foreign, &["init", "-q", "-b", "main"]);
    git(
        &foreign,
        &["remote", "add", "origin", upstream.to_str().unwrap()],
    );
    let root = base.join("mutable-parent").join("cache-root");
    std::fs::create_dir_all(&root).unwrap();
    let slot = root.join("aaaaaaaaaaaaaaaa");
    clone(upstream.to_str().unwrap(), &slot, DEFAULT_CLONE_MAX_BYTES).unwrap();
    std::fs::write(slot.join(MARKER_FILE), b"").unwrap();
    add_commit(&upstream, "next.txt", "next", "next commit");
    (upstream, slot, foreign)
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

include!("cache_tests/staging_sweep_tests.rs");

include!("cache_tests/polling_and_clone_limit_tests.rs");

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
    if crate::test_process::run_in_child() {
        return;
    }

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

include!("cache_tests/cache_worktree_shape_tests.rs");

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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

include!("cache_tests/eviction_ownership_tests.rs");

include!("cache_tests/bound_cache_object_tests.rs");

include!("cache_tests/windows_cache_path_tests.rs");

include!("cache_tests/cache_entry_and_size_tests.rs");

include!("cache_tests/directory_walk_race_tests.rs");

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.blocking_lock();
    let scratch = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

    let err =
        refetch_clone("https://example.invalid/never-cloned/repo").expect_err("no slot exists yet");
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
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.blocking_lock();
    let scratch = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

    let err = refetch_clone("https://user:tok3n@example.invalid/never-cloned/repo?token=SECRET")
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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
