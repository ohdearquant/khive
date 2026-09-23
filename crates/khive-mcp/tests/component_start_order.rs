#![cfg(unix)]

use clap::Parser;
use khive_mcp::args::Args;
use khive_mcp::server::KhiveMcpServer;
use khive_mcp::transport::TransportRegistry;
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn serve_entrypoints_do_not_start_components_before_ownership_or_in_client_roles() {
    for scenario in ["run-daemon", "server-daemon", "run-client", "server-client"] {
        let dir = tempfile::Builder::new()
            .prefix("kh-serve-")
            .tempdir_in("/tmp")
            .unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "component_start_order_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .envs(
                std::env::vars_os().filter(|(key, _)| !key.to_string_lossy().starts_with("KHIVE_")),
            )
            .env("HOME", dir.path())
            .env("KHIVE_COMPONENT_START_CASE", scenario)
            .env("KHIVE_SOCKET", dir.path().join("blocker/s"))
            .env("KHIVE_PID", dir.path().join("p"))
            .env("KHIVE_LOCK", dir.path().join("l"))
            .env("KHIVE_EVENTS_SPLIT", "0")
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let completed = loop {
            match child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    let _ = child.kill();
                    break false;
                }
            }
        };
        let output = child.wait_with_output().unwrap();
        assert!(
            completed && output.status.success(),
            "{scenario}: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("SERVE_COMPONENT_START_VERIFIED"),
            "{scenario}: {output:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "isolated child of serve_entrypoints_do_not_start_components_before_ownership_or_in_client_roles"]
async fn component_start_order_child() {
    let scenario = std::env::var("KHIVE_COMPONENT_START_CASE").unwrap();
    let daemon = scenario.ends_with("daemon");
    let blocker = khive_runtime::daemon::socket_path()
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::write(&blocker, b"cannot bind beneath a regular file").unwrap();
    let mut argv = vec![
        "mcp",
        "--db",
        ":memory:",
        "--no-embed",
        "--actor",
        "actor:component-start",
        "--pack",
        "kg",
        "--pack",
        "schedule",
        "--transport",
        "missing-test-transport",
    ];
    if daemon {
        argv.push("--daemon");
    }
    let args = Args::parse_from(argv);
    let transports = TransportRegistry::with_builtins();
    assert_eq!(khive_runtime::background_task_count(), 0);
    let result = if scenario.starts_with("run-") {
        khive_mcp::serve::run(args, &transports).await
    } else {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            actor_id: Some("actor:component-start".into()),
            brain_profile: None,
            packs: vec!["kg".into(), "schedule".into()],
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let server = KhiveMcpServer::new(runtime.clone()).unwrap();
        let guard = Some(khive_runtime::daemon::acquire_daemon_boot_guard().unwrap());
        khive_mcp::serve::serve_server(server, &args, &transports, guard, Some(runtime)).await
    };
    let error = result.expect_err("fixture cannot bind or resolve a transport");
    if daemon {
        assert!(khive_runtime::daemon_shutdown_token().is_cancelled());
        assert!(
            !error.to_string().contains("unknown transport"),
            "must reach daemon setup"
        );
    } else {
        assert!(error.to_string().contains("unknown transport"), "{error}");
        assert!(!khive_runtime::daemon_shutdown_token().is_cancelled());
    }
    assert_eq!(
        khive_runtime::background_task_count(),
        0,
        "an admitted schedule runtime must not start before ownership or in a client"
    );
    assert!(khive_mcp::components::component_health()
        .snapshot()
        .is_empty());
    println!("SERVE_COMPONENT_START_VERIFIED");
}
