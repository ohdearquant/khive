use std::process::Command;

fn write_multi_backend_config(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("selected-config.toml");
    std::fs::write(
        &path,
        r#"
[[backends]]
name = "main"
kind = "memory"

[[backends]]
name = "secondary"
kind = "memory"
"#,
    )
    .expect("write config");
    path
}

fn assert_exec_refusal(
    output: std::process::Output,
    config: &std::path::Path,
    override_path: &std::path::Path,
) {
    let selected_config = std::fs::canonicalize(config).expect("canonical selected config path");
    assert!(
        !output.status.success(),
        "refused invocation must exit nonzero"
    );
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be one JSON refusal envelope");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["invocation"]["started"], false);
    assert_eq!(
        envelope["error"]["code"],
        khive_mcp::serve::DB_OVERRIDE_CONFLICT_CODE
    );
    assert_eq!(
        envelope["error"]["config_path"],
        selected_config.display().to_string()
    );
    assert_eq!(
        envelope["error"]["db_override"],
        override_path.display().to_string()
    );
    assert_eq!(envelope["error"]["declared_backends"], 2);
    assert!(
        envelope.get("results").is_none(),
        "an invocation refusal must not masquerade as operation results"
    );
    assert!(
        envelope.get("summary").is_none(),
        "an invocation refusal must not carry an operation summary"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&selected_config.display().to_string()),
        "{stderr}"
    );
    assert!(stderr.contains("--config <path>"), "{stderr}");
    assert!(stderr.contains("KHIVE_CONFIG=<path>"), "{stderr}");
    assert!(stderr.contains("ephemeral"), "{stderr}");
    assert!(
        !override_path.exists(),
        "refusal must happen before the override database is opened"
    );
}

#[test]
fn exec_db_override_refusal_covers_config_flag_and_env() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("isolated home");
    let config = write_multi_backend_config(&dir);
    let override_path = dir.path().join("must-not-open.db");
    let binary = env!("CARGO_BIN_EXE_kkernel");

    let from_flag = Command::new(binary)
        .args([
            "exec",
            "stats()",
            "--config",
            config.to_str().expect("utf8 config path"),
            "--db",
            override_path.to_str().expect("utf8 db path"),
        ])
        .current_dir(dir.path())
        .env("HOME", home.path())
        .env("KHIVE_NO_DAEMON", "1")
        .env_remove("KHIVE_CONFIG")
        .env_remove("KHIVE_DB")
        .output()
        .expect("run kkernel exec with flags");
    assert_exec_refusal(from_flag, &config, &override_path);

    let from_env = Command::new(binary)
        .args(["exec", "stats()"])
        .current_dir(dir.path())
        .env("HOME", home.path())
        .env("KHIVE_NO_DAEMON", "1")
        .env("KHIVE_CONFIG", &config)
        .env("KHIVE_DB", &override_path)
        .output()
        .expect("run kkernel exec with environment overrides");
    assert_exec_refusal(from_env, &config, &override_path);
}

#[test]
fn exec_pending_events_db_override_refusal_emits_envelope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("isolated home");
    let config = write_multi_backend_config(&dir);
    let override_path = dir.path().join("must-not-open.db");
    let binary = env!("CARGO_BIN_EXE_kkernel");

    let from_flag = Command::new(binary)
        .args([
            "exec",
            "--pending-events",
            "--config",
            config.to_str().expect("utf8 config path"),
            "--db",
            override_path.to_str().expect("utf8 db path"),
        ])
        .current_dir(dir.path())
        .env("HOME", home.path())
        .env("KHIVE_NO_DAEMON", "1")
        .env_remove("KHIVE_CONFIG")
        .env_remove("KHIVE_DB")
        .output()
        .expect("run kkernel exec --pending-events with flags");
    assert_exec_refusal(from_flag, &config, &override_path);

    let from_env = Command::new(binary)
        .args(["exec", "--pending-events"])
        .current_dir(dir.path())
        .env("HOME", home.path())
        .env("KHIVE_NO_DAEMON", "1")
        .env("KHIVE_CONFIG", &config)
        .env("KHIVE_DB", &override_path)
        .output()
        .expect("run kkernel exec --pending-events with environment overrides");
    assert_exec_refusal(from_env, &config, &override_path);
}

#[test]
fn mcp_db_override_refusal_names_selected_config_without_protocol_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("isolated home");
    let config = write_multi_backend_config(&dir);
    let selected_config = std::fs::canonicalize(&config).expect("canonical selected config path");
    let override_path = dir.path().join("must-not-open.db");

    let output = Command::new(env!("CARGO_BIN_EXE_kkernel"))
        .args([
            "mcp",
            "--config",
            config.to_str().expect("utf8 config path"),
            "--db",
            override_path.to_str().expect("utf8 db path"),
            "--no-embed",
        ])
        .current_dir(dir.path())
        .env("HOME", home.path())
        .env_remove("KHIVE_CONFIG")
        .env_remove("KHIVE_DB")
        .output()
        .expect("run kkernel mcp");

    assert!(
        !output.status.success(),
        "refused invocation must exit nonzero"
    );
    assert!(
        output.stdout.is_empty(),
        "MCP startup errors must not write non-protocol JSON to stdout"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&selected_config.display().to_string()),
        "{stderr}"
    );
    assert!(stderr.contains("--config <path>"), "{stderr}");
    assert!(stderr.contains("KHIVE_CONFIG=<path>"), "{stderr}");
    assert!(
        !override_path.exists(),
        "refusal must happen before the override database is opened"
    );
}
