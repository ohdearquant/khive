//! Shared test-only fixtures for storage-adjacent crates.
//!
//! Gated behind the `test-support` feature so it never ships in a default
//! build; consumers pull it in as a dev-dependency.

use std::process::Command;

/// Freeze any lingering `-wal`/`-shm` sidecars for `path` by making them
/// read-only. Fixtures that close their SQLite connection asynchronously can
/// leave a writable sidecar behind; read-only admission rejects a writable
/// `-shm` as potentially live, so tests that reopen the file read-only freeze
/// the sidecars first to land on the documented frozen-snapshot form.
#[cfg(unix)]
pub fn freeze_snapshot_sidecars(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    for suffix in ["-wal", "-shm"] {
        let mut name = path.file_name().expect("db file name").to_os_string();
        name.push(suffix);
        let sidecar = path.parent().expect("db parent dir").join(name);
        if sidecar.exists() {
            let mut permissions = std::fs::metadata(&sidecar)
                .expect("sidecar metadata")
                .permissions();
            permissions.set_mode(0o444);
            std::fs::set_permissions(&sidecar, permissions).expect("freeze sidecar");
        }
    }
}

/// Run the calling test as the only test of a re-executed copy of the test
/// binary, so changes to the process environment never reach sibling tests.
///
/// The parent re-runs `current_exe()` with `--exact <test name> --nocapture
/// --test-threads=1` (plus `--include-ignored` when `include_ignored` is set),
/// names the test in the `marker` environment variable, and requires the child
/// to exit successfully after printing `test result: ok. 1 passed; 0 failed;`.
/// `configure` runs on the child command before the arguments and the marker
/// are applied, so a caller can set up or scrub the child environment but can
/// never displace either.
///
/// Returns `true` in the parent once the child passed: the caller returns from
/// its test. Returns `false` in the child, which continues with the real test
/// body. A child whose marker names this test but whose arguments differ from
/// the ones above panics before the test body runs.
pub fn run_exact_test_in_child(
    marker: &str,
    include_ignored: bool,
    configure: impl FnOnce(&mut Command),
) -> bool {
    let thread = std::thread::current();
    let name = thread.name().expect("libtest names its test threads");
    let mut arguments = vec!["--exact", name];
    if include_ignored {
        arguments.push("--include-ignored");
    }
    arguments.extend(["--nocapture", "--test-threads=1"]);
    if std::env::var(marker).ok().as_deref() == Some(name) {
        assert_eq!(
            std::env::args().skip(1).collect::<Vec<_>>(),
            arguments,
            "isolated child must run exactly its one named test"
        );
        return false;
    }

    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    configure(&mut command);
    let output = command
        .args(arguments)
        .env(marker, name)
        .output()
        .expect("spawn isolated test");
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .contains("test result: ok. 1 passed; 0 failed;"),
        "isolated test must execute exactly one passing case:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}
