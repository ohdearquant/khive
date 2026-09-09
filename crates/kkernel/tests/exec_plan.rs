use clap::{error::ErrorKind, Parser};
use kkernel::exec::ExecArgs;

#[test]
fn plan_accepts_positional_ops_without_default_presentation() {
    let args = ExecArgs::try_parse_from(["exec", "--plan", "create("])
        .expect("plan accepts raw operations for the daemon parser");
    assert_eq!(args.ops.as_deref(), Some("create("));
    assert_eq!(args.presentation, None);
}

#[test]
fn ordinary_exec_retains_verbose_presentation() {
    let args =
        ExecArgs::try_parse_from(["exec", "stats()"]).expect("ordinary operations remain accepted");
    assert_eq!(args.presentation.as_deref(), Some("verbose"));
}

#[test]
fn plan_requires_positional_ops() {
    let error = ExecArgs::try_parse_from(["exec", "--plan"]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn plan_rejects_explicit_execution_controls() {
    for option in [
        vec!["--presentation", "verbose"],
        vec!["--presentation", "agent"],
        vec!["--strict"],
        vec!["--output-format", "json"],
        vec!["--save-file", "results.jsonl"],
        vec!["--ops-file", "ops.jsonl"],
        vec!["--pending-events"],
        vec!["--dry-run"],
        vec!["--serial"],
        vec!["--atomic"],
        vec!["--atomic-max-ops", "10"],
        vec!["--verbose"],
        vec!["--actor", "test"],
        vec!["--expect-actor", "test"],
        vec!["--namespace", "local"],
    ] {
        let mut argv = vec!["exec", "--plan", "stats()"];
        argv.extend(option.iter().copied());
        let error = ExecArgs::try_parse_from(argv).unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::ArgumentConflict,
            "{option:?}: {error}"
        );
        assert!(error.to_string().contains(option[0]), "{error}");
    }
}

#[cfg(unix)]
mod daemon {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use khive_runtime::daemon::{read_frame, write_frame};
    use serde_json::{json, Value};
    use tempfile::TempDir;
    use tokio::net::UnixListener;
    use tokio::process::Command;

    fn canonical_plan(ops: &str) -> Value {
        let catalog = BTreeMap::from([
            ("stats".to_string(), "kg".to_string()),
            ("create".to_string(), "kg".to_string()),
            ("get".to_string(), "kg".to_string()),
            ("memory.remember".to_string(), "memory".to_string()),
        ]);
        khive_request::plan_request(ops, &catalog)
    }

    fn configured_command(temp: &TempDir, socket: &Path, ops: &str) -> Command {
        let config = temp.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
        command
            .args(["exec", "--plan", ops, "--config"])
            .arg(config)
            .arg("--db")
            .arg(temp.path().join("must-stay-absent.db"))
            .current_dir(temp.path())
            .env("KHIVE_SOCKET", socket)
            .env("KHIVE_PACKS", "kg,comm")
            .env("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1")
            .env("KHIVE_OUTPUT_FORMAT", "table")
            .env("KHIVE_PROCESS_REF", "ignored-plan-provenance")
            .env("KHIVE_TEST_HARNESS", "1")
            .env_remove("KHIVE_ACTOR")
            .env_remove("KHIVE_CONFIG")
            .env_remove("KHIVE_DB")
            .env_remove("KHIVE_EMBEDDING_MODEL")
            .env_remove("KHIVE_ADDITIONAL_EMBEDDING_MODELS")
            .kill_on_drop(true);
        command
    }

    fn response(frame: &Value, result: Value) -> Value {
        json!({
            "ok": true,
            "result": serde_json::to_string(&result).unwrap(),
            "error": null,
            "namespace_mismatch": false,
            "config_mismatch": false,
            "served_config_id": frame["config_id"],
            "version_mismatch": false,
            "daemon_protocol_version": 6
        })
    }

    async fn round_trip(ops: &str, result: Value) -> (std::process::Output, Value) {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let daemon_result = result.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let frame: Value =
                serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
            let reply = response(&frame, daemon_result);
            write_frame(&mut stream, &serde_json::to_vec(&reply).unwrap())
                .await
                .unwrap();
            frame
        });
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            configured_command(&temp, &socket, ops).output(),
        )
        .await
        .expect("plan must finish without fallback")
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let frame = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(!temp.path().join("must-stay-absent.db").exists());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            result
        );
        (output, frame)
    }

    #[tokio::test]
    async fn plan_returns_decoded_result_without_dispatch_rendering_or_identity() {
        for ops in [
            "stats()",
            "unknown_verb(value=1)",
            "[stats(), get(id=\"example\")]",
            "create(kind=\"note\", content=\"example\") | get(id=$prev.id) | memory.remember(source_id=$prev.id)",
            r#"{"tool":"get","args":{"id":"line\nbreak"}}"#,
        ] {
            let expected = canonical_plan(ops);
            assert_eq!(expected["parsed"], true, "fixture must parse: {ops}");
            let (_, frame) = round_trip(ops, expected).await;
            assert_eq!(frame["plan"], true);
            assert_eq!(frame["ops"], ops);
            assert_eq!(frame["namespace"], "");
            assert_eq!(frame["protocol_version"], 6);
            assert!(frame["config_id"].as_str().is_some_and(|id| !id.is_empty()));
            assert_eq!(frame.as_object().unwrap().len(), 5);
        }
    }

    #[tokio::test]
    async fn malformed_plan_is_a_successful_result() {
        let ops = "create(";
        let result = canonical_plan(ops);
        assert_eq!(result["parsed"], false);
        assert_eq!(
            result["error"],
            khive_request::parse_request(ops).unwrap_err().to_string()
        );
        round_trip(ops, result).await;
    }

    #[tokio::test]
    async fn previous_daemon_version_refuses_plan_without_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let dispatched = Arc::new(AtomicUsize::new(0));
        let daemon_dispatched = dispatched.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let frame: Value =
                serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
            let mismatch = frame["protocol_version"] != 4;
            if !mismatch {
                daemon_dispatched.fetch_add(1, Ordering::SeqCst);
            }
            let reply = json!({"ok":false,"result":null,"error":"version_mismatch",
                "namespace_mismatch":false,"config_mismatch":false,"served_config_id":frame["config_id"],
                "version_mismatch":mismatch,"daemon_protocol_version":4});
            write_frame(&mut stream, &serde_json::to_vec(&reply).unwrap())
                .await
                .unwrap();
        });
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            configured_command(
                &temp,
                &socket,
                "create(kind=\"note\", content=\"must not execute\")",
            )
            .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("version_mismatch"));
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(dispatched.load(Ordering::SeqCst), 0);
        assert!(!temp.path().join("must-stay-absent.db").exists());
    }

    #[tokio::test]
    async fn unavailable_daemon_fails_without_creating_a_database() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("absent.sock");
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            configured_command(&temp, &socket, "stats()").output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("already-running daemon"));
        assert!(!socket.exists());
        assert!(!temp.path().join("must-stay-absent.db").exists());
    }

    #[tokio::test]
    async fn plan_rejects_unconfirmed_daemon_responses_without_fallback() {
        for (field, replacement) in [
            ("config_mismatch", json!(true)),
            ("served_config_id", json!("different-config")),
            ("served_config_id", Value::Null),
            ("daemon_protocol_version", json!(4)),
            ("namespace_mismatch", json!(true)),
            ("result", Value::Null),
            ("result", json!("not JSON")),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let socket = temp.path().join("daemon.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let frame: Value =
                    serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
                let mut reply = response(&frame, json!({"parsed":true}));
                reply[field] = replacement;
                write_frame(&mut stream, &serde_json::to_vec(&reply).unwrap())
                    .await
                    .unwrap();
            });
            let output = tokio::time::timeout(
                Duration::from_secs(10),
                configured_command(&temp, &socket, "stats()").output(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(!output.status.success(), "accepted {field}");
            assert!(
                output.stdout.is_empty(),
                "{field}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            tokio::time::timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap();
            assert!(!temp.path().join("must-stay-absent.db").exists());
        }
    }
}
