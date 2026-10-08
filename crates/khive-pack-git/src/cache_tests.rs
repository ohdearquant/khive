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

#[test]
fn staging_sweep_skips_wrapper_removed_before_mtime_read() {
    let root = tempfile::tempdir().expect("tempdir");
    let (wrapper, _id, _held) = make_staging_wrapper(root.path(), false);
    let mut mtime_reads = 0;
    let removed = reap_stale_staging_with(
        root.path(),
        SystemTime::now(),
        Duration::from_secs(1),
        |path| {
            assert_eq!(path, wrapper.as_path());
            mtime_reads += 1;
            std::fs::remove_dir_all(path).expect("concurrent cleanup removes admitted wrapper");
            std::fs::symlink_metadata(path).and_then(|m| m.modified())
        },
    )
    .expect("a wrapper gone during liveness classification is skipped");

    assert_eq!(mtime_reads, 1, "the real sweep must reach the second stat");
    assert_eq!(removed, 0, "another cleanup's removal is not counted");
    assert_eq!(
        std::fs::symlink_metadata(&wrapper).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn staging_sweep_preserves_wrapper_mtime_errors() {
    let root = tempfile::tempdir().expect("tempdir");
    let (wrapper, _id, _held) = make_staging_wrapper(root.path(), false);
    let payload = wrapper.join("repo").join("payload");
    std::fs::write(&payload, b"keep").expect("fixture payload");
    let mut mtime_reads = 0;
    let error = reap_stale_staging_with(
        root.path(),
        SystemTime::now(),
        Duration::from_secs(1),
        |path| {
            assert_eq!(path, wrapper.as_path());
            mtime_reads += 1;
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected wrapper mtime denial",
            ))
        },
    )
    .expect_err("non-NotFound errors must still fail the sweep");

    match error {
        CacheError::Io(error) => {
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            let message = error.to_string();
            assert!(message.contains("reap_stale_staging: wrapper mtime"));
            assert!(message.contains(&wrapper.display().to_string()));
            assert!(message.contains("injected wrapper mtime denial"));
        }
        other => panic!("expected wrapper mtime I/O error, got {other:?}"),
    }
    assert_eq!(mtime_reads, 1);
    assert!(wrapper.is_dir());
    assert_eq!(std::fs::read(payload).unwrap(), b"keep");
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
    let removed = reap_stale_staging(root.path(), far_future, std::time::Duration::from_secs(1))
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
    let removed = reap_stale_staging(root.path(), far_future, std::time::Duration::from_secs(1))
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

/// A cheap walk (well under the floor) must not shrink the sleep below
/// `CLONE_SIZE_POLL_INTERVAL` -- a fast `dir_size` on a small clone keeps
/// polling at the same cadence it always has.
#[test]
fn poll_sleep_duration_floors_at_the_configured_interval() {
    assert_eq!(
        poll_sleep_duration(Duration::from_micros(1)),
        CLONE_SIZE_POLL_INTERVAL
    );
    assert_eq!(
        poll_sleep_duration(Duration::ZERO),
        CLONE_SIZE_POLL_INTERVAL
    );
}

/// Once the walk itself costs more than a quarter of the floor, the
/// sleep must grow past the floor and scale with the walk -- this is the
/// duty-cycle bound: the walk stays at or under a fifth of total loop
/// time even as the tree it measures grows arbitrarily large.
#[test]
fn poll_sleep_duration_grows_with_a_slow_walk() {
    assert_eq!(
        poll_sleep_duration(Duration::from_millis(100)),
        Duration::from_millis(400)
    );
    let slower = poll_sleep_duration(Duration::from_millis(200));
    let faster = poll_sleep_duration(Duration::from_millis(100));
    assert!(
        slower > faster,
        "a slower previous walk must yield a longer next sleep: {slower:?} vs {faster:?}"
    );
}

/// A `git clone` failure must not leave a staging wrapper behind.
#[test]
fn ensure_clone_cleans_up_staging_dir_on_clone_failure() {
    if crate::test_process::run_in_child() {
        return;
    }

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

/// Issue #2073: the clone cap must stop the transfer while `git clone`
/// is still running, not only reject the completed checkout afterwards.
#[cfg(unix)]
#[test]
fn ensure_clone_interrupts_an_inflight_transfer_at_the_size_cap() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let bin_dir = dir.path().join("bin");
    let finished = dir.path().join("clone-finished");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");

    let script = format!(
        r#"#!/bin/sh
for arg in "$@"; do dest="$arg"; done
mkdir -p "$dest/.git/objects/pack"
i=0
while [ "$i" -lt 128 ]; do
  dd if=/dev/zero of="$dest/.git/objects/pack/chunk-$i" bs=4096 count=1 2>/dev/null
  i=$((i + 1))
  sleep 0.01
done
printf 'finished\n' > '{}'
"#,
        finished.display()
    );
    let git_path = bin_dir.join("git");
    std::fs::write(&git_path, script).expect("write fake git");
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(&git_path)
        .expect("fake git metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&git_path, permissions).expect("chmod fake git");

    let prior_path = std::env::var("PATH").ok();
    let path = match &prior_path {
        Some(path) => format!("{}:{path}", bin_dir.display()),
        None => bin_dir.display().to_string(),
    };
    std::env::set_var("PATH", path);
    std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", dir.path().join("scratch"));
    std::env::set_var("KHIVE_GIT_DIGEST_CLONE_MAX_BYTES", "16384");
    let result = ensure_clone("https://example.invalid/oversized.git");
    match prior_path {
        Some(path) => std::env::set_var("PATH", path),
        None => std::env::remove_var("PATH"),
    }
    std::env::remove_var("KHIVE_GIT_DIGEST_CLONE_MAX_BYTES");
    std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");

    let err = result.expect_err("the in-flight clone must cross the configured cap");
    let bytes = match err {
        CacheError::CloneTooLarge { bytes, cap: 16_384 } => bytes,
        other => panic!("expected CloneTooLarge at the configured cap, got {other:?}"),
    };
    assert!(bytes > 16_384, "the measured transfer crossed the cap");
    assert!(
        bytes < 128 * 4096,
        "the child must be stopped before writing its full payload: {bytes}"
    );
    assert!(
        !finished.exists(),
        "the clone child must be terminated before its completion marker"
    );

    let namespace = staging_namespace_path(&dir.path().join("scratch"));
    let leftovers = std::fs::read_dir(namespace)
        .expect("read staging namespace")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .filter(|name| is_staging_wrapper_name(name))
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "failed clone left staging: {leftovers:?}"
    );
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

#[test]
fn evict_lru_only_touches_children_of_root() {
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    if crate::test_process::run_in_child() {
        return;
    }

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
    clone(url, &slot, clone_max_bytes()).expect("clone slot");
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
    if crate::test_process::run_in_child() {
        return;
    }

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
    clone(url, &slot, clone_max_bytes()).expect("clone slot");
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

/// Windows closes this race by preventing each rename, not by allowing
/// a rename and following an fd as the Unix regression above does.
#[cfg(windows)]
#[test]
fn issue2149_windows_pins_slot_and_git_child_through_fetch() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _guard = ENV_MUTEX.blocking_lock();
    for child in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (upstream, slot, foreign) = windows_fetchable_slot(dir.path());
        let validated = revalidate_owned_slot(&slot).expect("owned slot validates");
        let target = if child {
            slot.join(".git")
        } else {
            slot.clone()
        };
        let moved = target.with_file_name("moved-aside");
        assert!(
            std::fs::rename(&target, &moved).is_err(),
            "validated component must not be movable before git completes: {}",
            target.display()
        );
        assert!(target.is_dir());
        assert!(!moved.exists());
        // A wrong caller spelling must not override the validated path.
        // Foreign has a fetchable origin, so a pathname regression would
        // succeed there rather than fail for an unrelated setup reason.
        fetch(&foreign, &validated).expect("fetch into pinned slot");
        advance_to_fetched_tip(&foreign, &validated).expect("advance pinned refs");
        assert_eq!(head_sha(&slot), head_sha(&upstream));
        assert!(slot.join(".git/FETCH_HEAD").exists());
        assert!(!foreign.join(".git/FETCH_HEAD").exists());
        drop(validated);
        std::fs::rename(&target, &moved).expect("same rename succeeds after pin release");
    }
}

#[cfg(windows)]
#[test]
fn issue2149_windows_pins_every_git_path_ancestor() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _guard = ENV_MUTEX.blocking_lock();
    for parent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (upstream, slot, foreign) = windows_fetchable_slot(dir.path());
        let root = slot.parent().unwrap();
        let target = if parent { root.parent().unwrap() } else { root };
        let moved = target.with_file_name("ancestor-moved");
        let validated = revalidate_owned_slot(&slot).expect("owned slot validates");
        assert!(
            std::fs::rename(target, &moved).is_err(),
            "every directory in the Git command path must remain pinned"
        );
        fetch(&slot, &validated).unwrap();
        advance_to_fetched_tip(&slot, &validated).unwrap();
        assert_eq!(head_sha(&slot), head_sha(&upstream));
        assert!(!foreign.join(".git/FETCH_HEAD").exists());
        drop(validated);
        std::fs::rename(target, &moved).expect("ancestor can move after command pins drop");
    }
}

#[cfg(windows)]
#[test]
fn issue2149_windows_refuses_reparse_ownership_components() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _guard = ENV_MUTEX.blocking_lock();
    for component in ["slot", ".git", MARKER_FILE] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let slot = make_owned_entry(&root, "aaaaaaaaaaaaaaaa", true);
        let target = if component == "slot" {
            slot.clone()
        } else {
            slot.join(component)
        };
        let moved = target.with_file_name("unowned-target");
        std::fs::rename(&target, &moved).unwrap();
        if component == MARKER_FILE {
            std::fs::write(&moved, b"foreign marker must survive").unwrap();
            std::os::windows::fs::symlink_file(&moved, &target).expect(
                "Windows CI needs Developer Mode or symlink privilege; do not skip this witness",
            );
        } else {
            std::os::windows::fs::symlink_dir(&moved, &target).expect(
                "Windows CI needs Developer Mode or symlink privilege; do not skip this witness",
            );
        }
        assert!(matches!(
            revalidate_owned_slot(&slot),
            Err(CacheError::UnsafeToReplace(_))
        ));
        assert!(moved.exists(), "revalidation never removes foreign data");
        if component == MARKER_FILE {
            assert_eq!(
                std::fs::read(&moved).unwrap(),
                b"foreign marker must survive"
            );
            std::fs::remove_file(&target).unwrap();
        } else {
            std::fs::remove_dir(&target).unwrap();
        }
    }
}

#[cfg(windows)]
#[test]
fn issue2149_windows_resolved_path_survives_a_scratch_alias_swap() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().unwrap();
    let (upstream, slot, foreign) = windows_fetchable_slot(dir.path());
    let alias = dir.path().join("scratch-alias");
    let foreign_root = dir.path().join("foreign-root");
    std::fs::create_dir_all(&foreign_root).unwrap();
    let foreign_slot = foreign_root.join(slot.file_name().unwrap());
    std::os::windows::fs::symlink_dir(&foreign, &foreign_slot)
        .expect("Windows CI needs Developer Mode or symlink privilege; do not skip this witness");
    std::os::windows::fs::symlink_dir(slot.parent().unwrap(), &alias)
        .expect("Windows CI needs Developer Mode or symlink privilege; do not skip this witness");
    let alias_slot = alias.join(slot.file_name().unwrap());
    let validated = revalidate_owned_slot(&alias_slot).expect("resolved scratch alias validates");
    // Pins cover the resolved Git path, not this original alias. Replacing
    // the alias must therefore succeed and still have no effect on Git.
    std::fs::remove_dir(&alias).unwrap();
    // The replacement has the same slot name and a fetchable repository,
    // so reusing the original spelling would mutate foreign FETCH_HEAD.
    std::os::windows::fs::symlink_dir(&foreign_root, &alias).unwrap();
    fetch(&alias_slot, &validated).unwrap();
    advance_to_fetched_tip(&alias_slot, &validated).unwrap();
    assert_eq!(head_sha(&slot), head_sha(&upstream));
    assert!(!foreign.join(".git/FETCH_HEAD").exists());
    drop(validated);
    std::fs::remove_dir(&alias).unwrap();
    std::fs::remove_dir(&foreign_slot).unwrap();
}

#[cfg(windows)]
#[test]
fn issue2149_windows_over_cap_cleanup_releases_pins() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().unwrap();
    let upstream = dir.path().join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    init_origin_with_one_commit(&upstream);
    let root = dir.path().join("cache-root");
    let _scratch = ScratchRootGuard::set(&root);
    let url = upstream.to_str().unwrap();
    for refetch in [false, true] {
        std::env::set_var(
            "KHIVE_GIT_DIGEST_CLONE_MAX_BYTES",
            DEFAULT_CLONE_MAX_BYTES.to_string(),
        );
        let slot = ensure_clone(url).expect("create a normal owned slot");
        std::env::set_var("KHIVE_GIT_DIGEST_CLONE_MAX_BYTES", "1");
        let result = if refetch {
            refetch_clone(url)
        } else {
            ensure_clone(url)
        };
        assert!(
            matches!(result, Err(CacheError::CloneTooLarge { .. })),
            "over-cap cleanup must complete, got {result:?}"
        );
        assert!(
            !slot.exists(),
            "pins must not prevent owned over-cap removal"
        );
    }
    std::env::remove_var("KHIVE_GIT_DIGEST_CLONE_MAX_BYTES");
}

/// no-re-read guarantee: once the slot state is decided `Absent`, a
/// foreign directory appearing at the pathname must not be fetched
/// into, overwritten, or claimed.
#[test]
fn install_fresh_clone_refuses_a_foreign_occupied_slot() {
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
    if crate::test_process::run_in_child() {
        return;
    }

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
