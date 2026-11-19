#[cfg(unix)]
fn write_writable_multi_backend_config(
    config_path: &Path,
    main_path: &Path,
    secondary_path: &Path,
) {
    std::fs::write(
        config_path,
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "archive"
kind = "sqlite"
path = "{}"
"#,
            main_path.display(),
            secondary_path.display(),
        ),
    )
    .unwrap();
}

#[cfg(unix)]
fn chmod_read_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(path, permissions).unwrap();

    // A writable fixture's connections can close asynchronously and leave
    // `-wal`/`-shm` sidecars behind; read-only admission rejects a writable
    // `-shm` as potentially live. Freeze any lingering sidecars so the
    // snapshot takes the documented frozen form.
    for suffix in ["-wal", "-shm"] {
        let mut name = path.file_name().unwrap().to_os_string();
        name.push(suffix);
        let sidecar = path.parent().unwrap().join(name);
        if sidecar.exists() {
            let mut sidecar_permissions = std::fs::metadata(&sidecar).unwrap().permissions();
            sidecar_permissions.set_mode(0o444);
            std::fs::set_permissions(&sidecar, sidecar_permissions).unwrap();
        }
    }
}

#[cfg(unix)]
fn runtime_config_for_explicit_multi_backend(
    config_path: &Path,
    db: Option<&str>,
) -> RuntimeConfig {
    resolve_runtime_config(RuntimeConfigInputs {
        db,
        config: Some(config_path),
        namespace: Namespace::local(),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .unwrap()
}

/// A client must not reuse a daemon that retained a write-capable handle
/// after the declared main file was chmod'd into snapshot mode. The
/// filesystem-mode refusal belongs before the forwarding seam.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn multi_backend_main_chmod_refuses_before_daemon_forward() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

    let fixture = tempfile::tempdir().unwrap();
    let config_path = fixture.path().join("khive.toml");
    let main_path = fixture.path().join("main.db");
    let archive_path = fixture.path().join("archive.db");
    std::fs::write(&main_path, b"main snapshot fixture").unwrap();
    std::fs::write(&archive_path, b"archive fixture").unwrap();
    chmod_read_only(&main_path);
    write_writable_multi_backend_config(&config_path, &main_path, &archive_path);

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        runtime_config_for_explicit_multi_backend(&config_path, None),
        None,
        None,
        None,
        ExecDbContext {
            raw: None,
            anchor: None,
            config: Some(config_path),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    let error = result.expect_err("an undeclared main snapshot mode must fail closed");
    assert!(error.to_string().contains("read_only = true"), "{error}");
    assert!(
        SPY_CAPTURED_CONFIG_ID.with(|captured| captured.borrow().is_none()),
        "the retained writable daemon must never receive the frame"
    );
}

/// The topology fingerprint includes secondary modes too; apply the same
/// pre-forward refusal to every declared SQLite backend, not only `main`.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn multi_backend_secondary_chmod_refuses_before_daemon_forward() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

    let fixture = tempfile::tempdir().unwrap();
    let config_path = fixture.path().join("khive.toml");
    let main_path = fixture.path().join("main.db");
    let archive_path = fixture.path().join("archive.db");
    std::fs::write(&main_path, b"main fixture").unwrap();
    std::fs::write(&archive_path, b"archive snapshot fixture").unwrap();
    chmod_read_only(&archive_path);
    write_writable_multi_backend_config(&config_path, &main_path, &archive_path);

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        runtime_config_for_explicit_multi_backend(&config_path, None),
        None,
        None,
        None,
        ExecDbContext {
            raw: None,
            anchor: None,
            config: Some(config_path),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    let error = result.expect_err("an undeclared secondary snapshot mode must fail closed");
    assert!(error.to_string().contains("archive"), "{error}");
    assert!(
        SPY_CAPTURED_CONFIG_ID.with(|captured| captured.borrow().is_none()),
        "the retained writable daemon must never receive the frame"
    );
}

/// `--db :memory:` supersedes every declared file. Its pre-open/runtime
/// parity must therefore skip filesystem-mode checks on those unused paths.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn force_memory_skips_declared_chmod_preflight_and_forwards() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

    let fixture = tempfile::tempdir().unwrap();
    let config_path = fixture.path().join("khive.toml");
    let main_path = fixture.path().join("main.db");
    let archive_path = fixture.path().join("archive.db");
    std::fs::write(&main_path, b"unused main snapshot fixture").unwrap();
    std::fs::write(&archive_path, b"unused archive snapshot fixture").unwrap();
    chmod_read_only(&main_path);
    chmod_read_only(&archive_path);
    write_writable_multi_backend_config(&config_path, &main_path, &archive_path);

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        runtime_config_for_explicit_multi_backend(&config_path, Some(":memory:")),
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(":memory:".to_string()),
            anchor: None,
            config: Some(config_path),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    assert!(
        result.is_ok(),
        "force-memory forwarding must remain valid: {result:?}"
    );
    assert!(
        SPY_CAPTURED_CONFIG_ID.with(|captured| captured.borrow().is_some()),
        "the force-memory frame must reach the forwarding seam"
    );
}
