//! Real Git fixture under the production cache path; child stderr is the daemon log surrogate.
use super::*;
use std::os::unix::fs::PermissionsExt;

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cache_git_operations_never_inherit_daemon_stderr() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-q"]);
    git(
        &source,
        &[
            "-c",
            "user.name=Git Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    );
    let native = Command::new("/usr/bin/which").arg("git").output().unwrap();
    assert!(native.status.success());
    let native = String::from_utf8(native.stdout).unwrap();
    let shim_dir = root.path().join("shim");
    std::fs::create_dir(&shim_dir).unwrap();
    let shim = shim_dir.join("git");
    std::fs::write(
        &shim,
        r#"#!/bin/sh
printf '%s\n' "$@" >> "$KHIVE_STDERR_ARGV"
for arg in "$@"; do
  case "$arg" in
    clone|fetch|remote|update-ref)
      printf 'unstructured-git-progress-%s\rUpdating files: 37%%\r' "$arg" >&2 ;;
  esac
  if [ "$arg" = update-ref ] && [ "${KHIVE_STDERR_FAIL_UPDATE:-}" = yes ]; then
    exit 1
  fi
done
exec "$KHIVE_STDERR_NATIVE_GIT" "$@"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let argv_path = root.path().join("argv");
    let path = std::env::join_paths(
        std::iter::once(shim_dir).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let output = crate::test_process::command("cache::stderr_tests::cache_git_log_child")
        .env("PATH", path)
        .env("KHIVE_STDERR_NATIVE_GIT", native.trim())
        .env("KHIVE_STDERR_SOURCE", &source)
        .env("KHIVE_STDERR_DEST", root.path().join("clone"))
        .env("KHIVE_STDERR_ARGV", &argv_path)
        .output()
        .unwrap();
    crate::test_process::assert_success(&output);
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "INFO before cache operations\nINFO after cache operations\n",
        "Git progress must never share the daemon's stderr stream"
    );
    let args = std::fs::read_to_string(argv_path).unwrap();
    assert_eq!(
        args.lines().filter(|arg| *arg == "--no-progress").count(),
        3,
        "clone, fetch and refetch must all suppress progress"
    );
    for operation in ["clone", "fetch", "remote", "update-ref"] {
        assert!(
            args.lines().any(|arg| arg == operation),
            "fixture never exercised {operation}"
        );
    }
}

#[test]
#[ignore = "invoked by the isolated parent with fixture paths"]
fn cache_git_log_child() {
    let source = std::env::var("KHIVE_STDERR_SOURCE").unwrap();
    let dest = PathBuf::from(std::env::var_os("KHIVE_STDERR_DEST").unwrap());
    eprintln!("INFO before cache operations");
    clone(&source, &dest, u64::MAX).unwrap();
    let slot = ValidatedSlot::for_test(&dest);
    fetch(&dest, &slot).unwrap();
    fetch_refetch(&dest, &slot).unwrap();
    advance_to_fetched_tip(&dest, &slot).unwrap();
    let head = git_at_slot(&dest, &slot)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(head.status.success());
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim().len(), 40);
    // Preserve the operation's failure result even when stderr is discarded.
    std::env::set_var("KHIVE_STDERR_FAIL_UPDATE", "yes");
    let error = advance_to_fetched_tip(&dest, &slot).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("stale HEAD would walk stale history"),
        "{error}"
    );
    eprintln!("INFO after cache operations");
}
