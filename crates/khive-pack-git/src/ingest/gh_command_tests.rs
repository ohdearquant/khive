use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::{
    gh_json_with_command, probe_gh_repository_with_command,
    spawn_retrying_executable_busy_async as spawn_retrying_executable_busy, GhProbeError,
    IngestCommandError, OriginIdentity, EXECUTABLE_BUSY_BACKOFF_MS,
};

fn executable(dir: &Path, body: &str) -> PathBuf {
    let program = dir.join("fake-gh");
    std::fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    program
}

#[tokio::test(flavor = "current_thread")]
async fn busy_executable_spawn_retries_only_within_its_budget() {
    let mut attempts = 0;
    let value = spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || {
        attempts += 1;
        if attempts < 3 {
            Err(std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy))
        } else {
            Ok(17)
        }
    })
    .await
    .unwrap();
    assert_eq!((value, attempts), (17, 3));

    let mut denied_attempts = 0;
    let denied = spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || {
        denied_attempts += 1;
        Err::<(), _>(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    })
    .await
    .unwrap_err();
    assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(denied_attempts, 1);

    let mut exhausted_attempts = 0;
    let exhausted = spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || {
        exhausted_attempts += 1;
        Err::<(), _>(std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy))
    })
    .await
    .unwrap_err();
    assert_eq!(exhausted.kind(), std::io::ErrorKind::ExecutableFileBusy);
    assert_eq!(exhausted_attempts, 4);
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn page_fixture_survives_a_transient_busy_executable() {
    let test_name = std::thread::current()
        .name()
        .expect("libtest names test threads")
        .to_owned();
    if std::env::var("KHIVE_GIT_BUSY_FIXTURE_CHILD")
        .ok()
        .as_deref()
        != Some(test_name.as_str())
    {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                test_name.as_str(),
                "--nocapture",
                "--test-threads=1",
            ])
            .env("KHIVE_GIT_BUSY_FIXTURE_CHILD", &test_name)
            .env("HOME", &home)
            .output()
            .expect("spawn isolated busy-executable fixture");
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed;"),
            "busy-executable fixture failed: {output:?}"
        );
        assert!(std::fs::read_dir(home).unwrap().next().is_none());
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let program = executable(dir.path(), "printf '[]'");
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&program)
        .unwrap();
    let refused = tokio::process::Command::new(&program).spawn().unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::ExecutableFileBusy);

    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(writer);
    });
    let output = gh_json_with_command(
        dir.path(),
        "fixture/repository",
        &["pr", "list"],
        &program,
        Duration::from_secs(1),
        128,
    )
    .await
    .expect("the busy fixture should run once its writer closes");
    release.await.unwrap();
    assert_eq!(output, "[]");
}

#[tokio::test(flavor = "current_thread")]
async fn gh_page_timeout_is_typed_while_executor_keeps_running() {
    let dir = tempfile::tempdir().unwrap();
    let program = executable(dir.path(), "exec sleep 5");
    let ticked = Arc::new(AtomicBool::new(false));
    let pulse = Arc::clone(&ticked);
    let tick = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        pulse.store(true, Ordering::SeqCst);
    });
    let error = gh_json_with_command(
        dir.path(),
        "fixture/repository",
        &["issue", "list"],
        &program,
        Duration::from_millis(200),
        128,
    )
    .await
    .expect_err("a hung gh page must time out");
    assert_eq!(
        error.downcast_ref::<IngestCommandError>(),
        Some(&IngestCommandError::TimedOut)
    );
    assert!(
        ticked.load(Ordering::SeqCst),
        "the runtime worker must remain available during a hung gh page"
    );
    tick.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn gh_probe_timeout_is_typed() {
    let dir = tempfile::tempdir().unwrap();
    let program = executable(dir.path(), "exec sleep 5");
    let error = probe_gh_repository_with_command(
        dir.path(),
        Some("fixture/repository"),
        OriginIdentity::Never,
        &program,
        Duration::from_millis(100),
    )
    .await
    .expect_err("a hung repository probe must time out");
    assert_eq!(error, GhProbeError::GhTimedOut);
}

#[tokio::test]
async fn gh_page_stdout_is_bounded_before_json_parsing() {
    let dir = tempfile::tempdir().unwrap();
    let program = executable(
        dir.path(),
        "printf '1234567890123456789012345678901234567890'",
    );
    let error = gh_json_with_command(
        dir.path(),
        "fixture/repository",
        &["pr", "list"],
        &program,
        Duration::from_secs(1),
        16,
    )
    .await
    .expect_err("an oversized page must be refused");
    assert_eq!(
        error.downcast_ref::<IngestCommandError>(),
        Some(&IngestCommandError::StdoutTooLarge)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn page_timeout_reason_survives_plain_display() {
    let dir = tempfile::tempdir().unwrap();
    let program = executable(dir.path(), "exec sleep 5");
    let error = gh_json_with_command(
        dir.path(),
        "fixture/repository",
        &["issue", "list"],
        &program,
        Duration::from_millis(200),
        128,
    )
    .await
    .expect_err("timeout fixture must fail");
    assert_eq!(
        error.downcast_ref::<IngestCommandError>(),
        Some(&IngestCommandError::TimedOut)
    );
    assert!(
        error.to_string().contains("command timed out"),
        "public report loses timeout cause: {error}"
    );
}

#[tokio::test]
async fn page_stdout_limit_reason_survives_plain_display() {
    let dir = tempfile::tempdir().unwrap();
    let program = executable(
        dir.path(),
        "printf '1234567890123456789012345678901234567890'",
    );
    let error = gh_json_with_command(
        dir.path(),
        "fixture/repository",
        &["pr", "list"],
        &program,
        Duration::from_secs(5),
        16,
    )
    .await
    .expect_err("overflow fixture must fail");
    assert_eq!(
        error.downcast_ref::<IngestCommandError>(),
        Some(&IngestCommandError::StdoutTooLarge)
    );
    assert!(
        error.to_string().contains("command stdout exceeded limit"),
        "public report loses stdout limit cause: {error}"
    );
}

#[tokio::test]
async fn page_stderr_limit_reason_survives_plain_display_without_payload() {
    let dir = tempfile::tempdir().unwrap();
    let sentinel = "SYNTHETIC_STDERR_NOT_A_SECRET";
    let payload = format!("{sentinel}{}", "x".repeat(super::GH_STDERR_LIMIT + 1));
    let program = executable(
        dir.path(),
        &format!("printf '%s' '{payload}' >&2\nprintf '[]'"),
    );
    let error = gh_json_with_command(
        dir.path(),
        "fixture/repository",
        &["issue", "list"],
        &program,
        Duration::from_secs(5),
        16,
    )
    .await
    .expect_err("stderr overflow must fail");
    assert_eq!(
        error.downcast_ref::<IngestCommandError>(),
        Some(&IngestCommandError::StderrTooLarge)
    );
    let display = error.to_string();
    assert!(
        display.contains("command stderr exceeded limit"),
        "public report loses stderr limit cause: {display}"
    );
    assert!(
        !display.contains(sentinel),
        "stderr payload must remain private"
    );
}

#[tokio::test]
async fn page_missing_program_reason_survives_plain_display() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent-synthetic-gh");
    let error = gh_json_with_command(
        dir.path(),
        "fixture/repository",
        &["issue", "list"],
        &missing,
        Duration::from_secs(5),
        16,
    )
    .await
    .expect_err("missing program must fail");
    assert_eq!(
        error.downcast_ref::<IngestCommandError>(),
        Some(&IngestCommandError::NotFound)
    );
    assert!(
        error.to_string().contains("command not found on PATH"),
        "public report loses start failure cause: {error}"
    );
}

#[tokio::test]
async fn page_exact_stdout_limit_still_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let program = executable(dir.path(), "printf '[]'");
    let output = gh_json_with_command(
        dir.path(),
        "fixture/repository",
        &["issue", "list"],
        &program,
        Duration::from_secs(5),
        2,
    )
    .await
    .expect("exact-limit output must remain accepted");
    assert_eq!(output, "[]");
}

#[tokio::test]
async fn page_nonzero_exit_still_omits_stderr_payload() {
    let dir = tempfile::tempdir().unwrap();
    let sentinel = "SYNTHETIC_STDERR_NOT_A_SECRET";
    let program = executable(
        dir.path(),
        &format!("printf '%s' '{sentinel}' >&2\nexit 23"),
    );
    let error = gh_json_with_command(
        dir.path(),
        "fixture/repository",
        &["issue", "list"],
        &program,
        Duration::from_secs(5),
        16,
    )
    .await
    .expect_err("nonzero fixture must fail");
    assert_eq!(error.to_string(), "gh issue list failed");
    assert!(!error.to_string().contains(sentinel));
}
