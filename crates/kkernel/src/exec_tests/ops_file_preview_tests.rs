#[tokio::test]
async fn ops_file_dry_run_writes_nothing() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let mut f = NamedTempFile::new().unwrap();
    use std::io::Write as _;
    for name in ["DryA", "DryB"] {
        let line = format!(
            "{{\"tool\":\"create\",\"args\":{{\"kind\":\"concept\",\"name\":\"{name}\"}}}}\n"
        );
        f.write_all(line.as_bytes()).unwrap();
    }

    let path = f.path().to_path_buf();
    let cfg = RuntimeConfig {
        db_path: Some(PathBuf::from(&db_path)),
        ..Default::default()
    };

    // dry_run=true → no writes.
    run_exec_ops_file(
        path.clone(),
        cfg.clone(),
        None,
        None,
        None,
        true,
        ExecDbContext::default(),
        false,
        false,
        None,
        false,
    )
    .await
    .unwrap();

    // Verify nothing was written by checking with a fresh server.
    let server = isolated_server(&db_path);
    let params = RequestParams {
        plan: None,
        ops: r#"list(kind="concept")"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
        request_id: None,
    };
    let raw = server.dispatch_request_local(params).await.unwrap();
    let resp: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let count = resp["results"][0]["result"]["items"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(count, 0, "dry-run must not write any entities");
}
