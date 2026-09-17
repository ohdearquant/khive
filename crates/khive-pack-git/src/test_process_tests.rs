use std::process::Command;

use crate::test_process::{command, passed_one_test, run_in_child, run_in_child_with};

#[cfg(unix)]
#[test]
fn hostile_child_environment_cannot_change_git_observers() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    let keys = [
        "PATH",
        "KHIVE_GIT_ISOLATED_TEST",
        "KHIVE_GIT_DIGEST_SCRATCH_ROOT",
        "KHIVE_GIT_DIGEST_CACHE_MAX_REPOS",
        "KHIVE_GIT_DIGEST_CACHE_MAX_BYTES",
        "KHIVE_GIT_DIGEST_CLONE_MAX_BYTES",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
    ];
    let before: Vec<_> = keys.iter().map(std::env::var_os).collect();
    let native_version = Command::new("git").arg("--version").output().unwrap();
    assert!(native_version.status.success());
    let dir = tempfile::tempdir().unwrap();
    let shim = dir.path().join("git");
    std::fs::write(
        &shim,
        "#!/bin/sh\nprintf 'hostile-git-every-call\\n'\nexit 97\n",
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ready = dir.path().join("ready");
    let release = dir.path().join("release");
    const HOSTILE_CASE: &str = "test_process_tests::hostile_environment_child";
    let mut hostile = command(HOSTILE_CASE);
    hostile
        .env("KHIVE_TEST_CHILD_SHIM", dir.path())
        .env("KHIVE_TEST_CHILD_READY", &ready)
        .env("KHIVE_TEST_CHILD_RELEASE", &release)
        .env("KHIVE_TEST_CHILD_FAIL", "false");
    let (observers, concurrent_version) = std::thread::scope(|scope| {
        let ready_child = ready.clone();
        let release_child = release.clone();
        let shim_dir = dir.path();
        let child = std::thread::Builder::new()
            .name(HOSTILE_CASE.into())
            .spawn_scoped(scope, move || {
                if run_in_child_with(hostile) {
                    return;
                }
                hostile_environment(shim_dir, &ready_child, &release_child, false);
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ready.exists(),
            "hostile child did not reach its shim assertions"
        );
        let concurrent_version = Command::new("git").arg("--version").output().unwrap();
        let observers: Vec<_> = [
            "source::tests::repo_identity_local_path_with_origin_matches_remote_identity",
            "source::tests::repo_identity_local_path_with_bracketed_ipv6_scp_origin_converges",
            "source::tests::repo_identity_local_path_without_remote_falls_back_to_path_form",
        ]
        .into_iter()
        .map(|name| (name, command(name).output().unwrap()))
        .collect();
        std::fs::write(&release, b"release").unwrap();
        child.join().unwrap();
        (observers, concurrent_version)
    });
    assert_eq!(
        keys.iter().map(std::env::var_os).collect::<Vec<_>>(),
        before
    );
    let failures: Vec<_> = observers
        .iter()
        .filter(|(_, output)| !passed_one_test(output))
        .map(|(name, output)| {
            format!(
                "{name}:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
        .collect();
    assert!(failures.is_empty(), "git observers changed: {failures:#?}");
    assert!(concurrent_version.status.success());
    assert_eq!(concurrent_version.stdout, native_version.stdout);

    let failed = command(HOSTILE_CASE)
        .env("KHIVE_TEST_CHILD_SHIM", dir.path())
        .env("KHIVE_TEST_CHILD_READY", &ready)
        .env("KHIVE_TEST_CHILD_RELEASE", &release)
        .env("KHIVE_TEST_CHILD_FAIL", "true")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stdout)
        .contains("test result: FAILED. 0 passed; 1 failed;"));
    assert!(String::from_utf8_lossy(&failed.stderr).contains("intentional isolated failure"));
    assert_eq!(
        keys.iter().map(std::env::var_os).collect::<Vec<_>>(),
        before
    );
    let missing = command("test_process_tests::this_test_does_not_exist")
        .output()
        .unwrap();
    assert!(missing.status.success());
    assert!(
        !passed_one_test(&missing),
        "zero selected tests must never pass isolation"
    );
    let broad = command(HOSTILE_CASE)
        .env("KHIVE_TEST_CHILD_SHIM", dir.path())
        .env("KHIVE_TEST_CHILD_READY", &ready)
        .env("KHIVE_TEST_CHILD_RELEASE", &release)
        .env("KHIVE_TEST_CHILD_FAIL", "false")
        .args(["--skip", "this_test_does_not_exist"])
        .output()
        .unwrap();
    assert!(!broad.status.success());
    assert!(String::from_utf8_lossy(&broad.stderr)
        .contains("isolated child must run exactly its one named test"));
    assert_eq!(
        keys.iter().map(std::env::var_os).collect::<Vec<_>>(),
        before
    );
}

#[cfg(unix)]
#[test]
#[ignore = "child fixture for hostile_child_environment_cannot_change_git_observers"]
fn hostile_environment_child() {
    if run_in_child() {
        return;
    }
    hostile_environment(
        std::path::Path::new(&std::env::var_os("KHIVE_TEST_CHILD_SHIM").unwrap()),
        std::path::Path::new(&std::env::var_os("KHIVE_TEST_CHILD_READY").unwrap()),
        std::path::Path::new(&std::env::var_os("KHIVE_TEST_CHILD_RELEASE").unwrap()),
        std::env::var("KHIVE_TEST_CHILD_FAIL").unwrap() == "true",
    );
}

#[cfg(unix)]
fn hostile_environment(
    shim_dir: &std::path::Path,
    ready: &std::path::Path,
    release: &std::path::Path,
    fail: bool,
) {
    use std::time::{Duration, Instant};

    struct RestorePath(Option<std::ffi::OsString>);
    impl Drop for RestorePath {
        fn drop(&mut self) {
            match self.0.take() {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
    }
    let _restore = RestorePath(std::env::var_os("PATH"));
    std::env::set_var("PATH", shim_dir);
    for args in [vec!["--version"], vec!["rev-parse", "HEAD"]] {
        let output = Command::new("git").args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(97));
        assert_eq!(output.stdout, b"hostile-git-every-call\n");
    }
    std::fs::write(ready, b"ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !release.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        release.exists(),
        "parent did not finish its concurrent observers"
    );
    assert!(!fail, "intentional isolated failure");
}
