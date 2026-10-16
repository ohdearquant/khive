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
