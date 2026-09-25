#![cfg(unix)]

use khive_runtime::daemon::{
    read_frame, write_frame, DaemonRequestFrame, DaemonResponseFrame, PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio::process::Command;

const DISCLOSURE: &str = "execution: answered by daemon; --log and KHIVE_LOG set the client process log level only; the daemon log level is fixed at startup";
const WATCHDOG: Duration = Duration::from_secs(20);

fn fixture() -> TempDir {
    tempfile::Builder::new()
        .prefix("exec-log-")
        .tempdir_in("/tmp")
        .expect("short isolated socket directory")
}

fn exec_command(root: &TempDir) -> Command {
    let config = root.path().join("khive.toml");
    std::fs::write(&config, "[packs.kg]\nbackend = \"main\"\nno_embed = true\n").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("KHIVE_") {
            command.env_remove(name);
        }
    }
    command
        .args(["exec", "whoami()", "--actor", "actor:exec-log"])
        .arg("--config")
        .arg(config)
        .arg("--db")
        .arg(root.path().join("fixture.db"))
        .current_dir(root.path())
        .env("HOME", root.path())
        .env("KHIVE_PACKS", "kg")
        .env("KHIVE_EVENTS_SPLIT", "0")
        .env("KHIVE_SOCKET", root.path().join("s"))
        .env("KHIVE_PID", root.path().join("p"))
        .env("KHIVE_LOCK", root.path().join("l"))
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn forwarded_exec_discloses_daemon_logging_scope() {
    let root = fixture();
    let listener = UnixListener::bind(root.path().join("s")).unwrap();
    let daemon = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let bytes = read_frame(&mut stream).await.unwrap();
        let frame: DaemonRequestFrame = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(frame.ops, "whoami()");
        assert_eq!(frame.actor_id.as_deref(), Some("actor:exec-log"));
        let response = DaemonResponseFrame {
            ok: true,
            result: Some(json!({"daemon_disclosure_fixture": true}).to_string()),
            error: None,
            error_detail: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(frame.config_id),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: frame.request_id,
        };
        write_frame(&mut stream, &serde_json::to_vec(&response).unwrap())
            .await
            .unwrap();
    };
    let mut command = exec_command(&root);
    let (served, output) = tokio::join!(tokio::time::timeout(WATCHDOG, daemon), async {
        let output = tokio::time::timeout(WATCHDOG, command.output())
            .await
            .expect("exec must finish")
            .expect("run exec");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        output
    },);
    served.expect("client must reach the fake daemon");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"daemon_disclosure_fixture": true}),
        "disclosure must not replace or decorate the daemon's stdout payload"
    );
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.starts_with("execution:"))
            .collect::<Vec<_>>(),
        vec![DISCLOSURE],
        "stderr={stderr}"
    );
}

#[tokio::test]
async fn in_process_exec_omits_daemon_logging_disclosure() {
    let root = fixture();
    let output = tokio::time::timeout(
        WATCHDOG,
        exec_command(&root).env("KHIVE_NO_DAEMON", "1").output(),
    )
    .await
    .expect("local exec must finish")
    .expect("run local exec");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["results"][0]["ok"], true);
    assert_eq!(
        response["results"][0]["result"]["actor_id"],
        "actor:exec-log"
    );
    assert!(!stderr.contains("answered by daemon"), "{stderr}");
    assert!(
        !stderr.contains("client process log level only"),
        "{stderr}"
    );
}
