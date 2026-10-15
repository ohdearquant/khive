#[tokio::test]
async fn private_client_bootstraps_while_default_supervisor_marker_exists() {
    let fixture = Fixture::new();
    let default_marker = fixture.home.join(".khive/khived.supervisor");
    let foreign = format!("default.job\n{}\n60\n", std::process::id());
    std::fs::write(&default_marker, &foreign).unwrap();
    let started = Instant::now();
    let (status, log) = fixture.completed(
        fixture
            .exec_command()
            .env_remove("KHIVE_SUPERVISOR_MARKER")
            .env("KHIVE_DAEMON_STRICT", "1"),
        "private-bootstrap.log",
    );
    assert!(status.success(), "private client failed: {log}");
    assert!(
        started.elapsed() < START_LIMIT,
        "unrelated marker delayed startup: {log}"
    );
    assert!(
        log.lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|response| response["summary"]["succeeded"] == 1
                && response["results"][0]["tool"] == "stats"
                && response["results"][0]["ok"] == true),
        "private daemon did not serve stats: {log}"
    );
    let pid = wait_for_holder(&fixture, None).await;
    assert!(fixture_daemon_alive(pid, &fixture.config));
    assert_eq!(std::fs::read_to_string(default_marker).unwrap(), foreign);
    assert!(!fixture.root.path().join("s.supervisor-marker").exists());
}

#[tokio::test]
async fn private_supervisor_publishes_and_releases_only_its_socket_marker() {
    let fixture = Fixture::new();
    let default_marker = fixture.home.join(".khive/khived.supervisor");
    let foreign = format!("default.job\n{}\n60\n", std::process::id());
    std::fs::write(&default_marker, &foreign).unwrap();
    let marker = fixture.root.path().join("s.supervisor-marker");
    let (mut launcher, log_path) = fixture.spawn(
        fixture
            .launch_command(&fixture.config)
            .env_remove("KHIVE_SUPERVISOR_MARKER"),
        "private-supervisor.log",
    );
    let pid = launcher.0.id();
    assert_eq!(wait_for_holder(&fixture, Some(pid)).await, pid);
    let before = assert_supervisor_marker(&marker, pid, 10);
    assert_eq!(std::fs::read_to_string(&default_marker).unwrap(), foreign);
    // SAFETY: this is the exact unreaped launcher child owned by the fixture.
    assert_eq!(
        unsafe { libc::kill(i32::try_from(pid).unwrap(), libc::SIGTERM) },
        0
    );
    let (status, log) = wait_child_exit(&mut launcher, &log_path);
    assert!(
        status.success(),
        "private supervisor did not stop cleanly: {log}"
    );
    assert!(!fixture.socket.exists());
    assert!(!fixture.pid_file.exists());
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), before);
    let (status, log) = fixture.completed(
        fixture
            .command()
            .env_remove("KHIVE_SUPERVISOR_MARKER")
            .args(["supervisor", "release", "--label", LABEL]),
        "private-release.log",
    );
    assert!(status.success(), "private release failed: {log}");
    assert!(!marker.exists());
    assert!(fixture
        .root
        .path()
        .join("s.supervisor-marker.lock")
        .exists());
    assert_eq!(std::fs::read_to_string(default_marker).unwrap(), foreign);
}
