//! Process identity belongs to the serving OS process, including when a new
//! process reports the same build and an otherwise identical empty backend.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use khive_db::diagnostics::{collect, BuildIdentity};
use khive_db::{ConnectionPool, PoolConfig};

const CHILD_OUTPUT: &str = "KHIVE_DIAGNOSTICS_PROCESS_IDENTITY_OUTPUT";

fn process_json() -> serde_json::Value {
    // An inspection-only in-memory pool avoids the writer-timeout sink and
    // touches no user's database or logs.
    let pool = ConnectionPool::new(PoolConfig {
        read_only: true,
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    })
    .expect("inspection pool");
    let report = collect(
        &pool,
        BuildIdentity::from_env("same-build", None),
        Duration::from_secs(30),
    );
    assert_eq!(report.process.pid, std::process::id());
    assert_eq!(
        report.process.started_at,
        khive_db::walpin::process_start_time_secs(std::process::id()),
        "the timestamp must come from the OS process, even on the first report"
    );
    serde_json::to_value(report).unwrap()["process"].clone()
}

#[test]
fn diagnostics_identifies_a_fresh_serving_process() {
    if let Some(output_path) = std::env::var_os(CHILD_OUTPUT) {
        let identity = process_json();
        assert_eq!(identity["pool_generation"], 1);
        let replacement = process_json();
        assert_eq!(replacement["pool_generation"], 2);
        assert_eq!(replacement["pid"], identity["pid"]);
        assert_eq!(replacement["started_at"], identity["started_at"]);
        std::fs::write(output_path, serde_json::to_vec(&identity).unwrap())
            .expect("write child identity");
        return;
    }

    let parent = process_json();
    let dir = tempfile::tempdir().expect("owned child output directory");
    let output_path = dir.path().join("process.json");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "diagnostics_identifies_a_fresh_serving_process",
            "--nocapture",
        ])
        .env(CHILD_OUTPUT, &output_path)
        .env("KHIVE_TEST_HARNESS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("fresh process");
    let child_pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("failed to observe child completion: {error}");
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("process identity child exceeded its 10-second completion deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Drain captured output only after observing exit, so a child regression
    // cannot park the parent in an unbounded wait for completion.
    let output = child.wait_with_output().expect("child completion");
    assert!(
        output.status.success(),
        "child failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let child_identity: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output_path).unwrap()).unwrap();
    assert_eq!(child_identity["pid"], child_pid);
    assert_ne!(child_identity["pid"], parent["pid"]);
    assert_ne!(child_identity, parent);
    assert_eq!(child_identity["pool_generation"], 1);
    // Whole-second start timestamps may match for two processes started in
    // the same second. The PID + available start time is the identity pair.
}
