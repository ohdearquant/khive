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

#[test]
fn exec_db_override_refusal_is_machine_readable_and_names_selected_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = write_multi_backend_config(&dir);
    let override_path = dir.path().join("must-not-open.db");

    let output = Command::new(env!("CARGO_BIN_EXE_kkernel"))
        .args([
            "exec",
            "stats()",
            "--config",
            config.to_str().expect("utf8 config path"),
            "--db",
            override_path.to_str().expect("utf8 db path"),
        ])
        .env_remove("KHIVE_CONFIG")
        .env_remove("KHIVE_DB")
        .output()
        .expect("run kkernel exec");

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
        config.display().to_string()
    );
    assert_eq!(envelope["error"]["declared_backends"], 2);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&config.display().to_string()), "{stderr}");
    assert!(stderr.contains("--config <path>"), "{stderr}");
    assert!(stderr.contains("KHIVE_CONFIG=<path>"), "{stderr}");
    assert!(
        !override_path.exists(),
        "refusal must happen before the override database is opened"
    );
}

#[test]
fn mcp_db_override_refusal_names_selected_config_without_protocol_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = write_multi_backend_config(&dir);
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
    assert!(stderr.contains(&config.display().to_string()), "{stderr}");
    assert!(stderr.contains("--config <path>"), "{stderr}");
    assert!(stderr.contains("KHIVE_CONFIG=<path>"), "{stderr}");
    assert!(
        !override_path.exists(),
        "refusal must happen before the override database is opened"
    );
}
