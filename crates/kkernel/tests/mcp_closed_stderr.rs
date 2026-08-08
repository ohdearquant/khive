//! Regression coverage for issue #1716: stderr is a diagnostic side channel,
//! not a liveness dependency of the stdin/stdout MCP transport.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn kkernel_binary() -> &'static str {
    env!("CARGO_BIN_EXE_kkernel")
}

fn kill_and_wait(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn stdio_mcp_survives_when_the_stderr_consumer_disconnects() {
    let home = tempfile::tempdir().expect("isolated HOME");
    let mut child = Command::new(kkernel_binary())
        .arg("mcp")
        .arg("--db")
        .arg(":memory:")
        .arg("--no-embed")
        .arg("--pack")
        .arg("kg")
        .env("HOME", home.path())
        // rmcp emits an initialization event at INFO after the request below.
        // Keeping that event enabled makes the regression deterministic even
        // if every startup diagnostic was already written before we close the
        // pipe.
        .env("KHIVE_LOG", "trace")
        .env_remove("KHIVE_CONFIG")
        .env_remove("KHIVE_DB")
        .env_remove("KHIVE_PACKS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kkernel mcp");

    let stderr = child.stderr.take().expect("piped stderr");
    let (first_log_tx, first_log_rx) = mpsc::sync_channel(1);
    let stderr_reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut logs = String::new();
        let result = loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break Ok(()),
                Ok(_) => {
                    let saw_database_disclosure = line.contains("database:");
                    logs.push_str(&line);
                    if saw_database_disclosure {
                        break Ok(());
                    }
                }
                Err(error) => break Err(error),
            }
        };
        // Returning drops the only consumer of the child's stderr pipe.
        first_log_tx.send((result, logs)).ok();
    });
    let (first_log_result, first_log) = match first_log_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(result) => result,
        Err(error) => {
            kill_and_wait(&mut child);
            panic!("timed out waiting for the startup diagnostic: {error}");
        }
    };
    first_log_result.expect("read startup diagnostic");
    assert!(
        first_log.contains("database:"),
        "expected the khive.boot database disclosure, got {first_log:?}"
    );
    stderr_reader.join().expect("stderr reader thread");

    let stdout = child.stdout.take().expect("piped stdout");
    let (response_tx, response_rx) = mpsc::sync_channel(2);
    let stdout_reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        for _ in 0..2 {
            let mut line = String::new();
            let result = reader.read_line(&mut line);
            let reached_eof = matches!(result, Ok(0));
            if response_tx.send((result, line)).is_err() || reached_eof {
                break;
            }
        }
    });

    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "closed-stderr-regression", "version": "1"}
        }
    });
    let stdin = child.stdin.as_mut().expect("piped stdin");
    writeln!(stdin, "{initialize}").expect("send initialize request");
    stdin.flush().expect("flush initialize request");

    let (response_result, response) = match response_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(result) => result,
        Err(error) => {
            kill_and_wait(&mut child);
            stdout_reader
                .join()
                .expect("stdout reader thread after kill");
            panic!("timed out waiting for initialize response: {error}");
        }
    };
    response_result.expect("read initialize response");

    let response: serde_json::Value = serde_json::from_str(&response).unwrap_or_else(|error| {
        let status = child.try_wait().expect("inspect child status");
        panic!("invalid initialize response {response:?} ({error}); child status: {status:?}")
    });
    assert_eq!(response["id"], 1);
    assert!(
        response.get("result").is_some(),
        "initialize must succeed after stderr disconnect: {response}"
    );

    // rmcp emits its "Service initialized as server" tracing event after it
    // has put the initialize response on stdout. A reverted writer can panic
    // after that first response is already buffered, so synchronize beyond the
    // event with another protocol round-trip instead of checking process state
    // in the race window.
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let ping = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "ping"});
    let stdin = child.stdin.as_mut().expect("piped stdin");
    writeln!(stdin, "{initialized}").expect("send initialized notification");
    writeln!(stdin, "{ping}").expect("send post-handshake ping");
    stdin.flush().expect("flush post-handshake requests");

    let (ping_result, ping_response) = match response_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(result) => result,
        Err(error) => {
            kill_and_wait(&mut child);
            stdout_reader
                .join()
                .expect("stdout reader thread after kill");
            panic!("timed out waiting for post-handshake ping response: {error}");
        }
    };
    ping_result.expect("read post-handshake ping response");
    let ping_response: serde_json::Value =
        serde_json::from_str(&ping_response).unwrap_or_else(|error| {
            let status = child.try_wait().expect("inspect child status");
            panic!(
                "invalid post-handshake response {ping_response:?} ({error}); child status: {status:?}"
            )
        });
    assert_eq!(ping_response["id"], 2);
    assert!(
        ping_response.get("result").is_some(),
        "post-handshake ping must succeed after stderr disconnect: {ping_response}"
    );
    assert_eq!(
        child.try_wait().expect("inspect MCP process"),
        None,
        "MCP server must remain alive after its stderr consumer disconnects"
    );

    kill_and_wait(&mut child);
    stdout_reader.join().expect("stdout reader thread");
}
