// MUST-FAIL: publishing a helper/parent PID instead of the exec'd process cannot
// satisfy the peer-credential equality, even if a socket happens to exist.
#[tokio::test]
async fn supervisor_launcher_marker_pid_is_the_actual_socket_owner() {
    let fixture = Fixture::new();
    let (mut launcher, log_path) =
        fixture.spawn(&mut fixture.launch_command(&fixture.config), "launch.log");
    let pid = launcher.0.id();
    let deadline = tokio::time::Instant::now() + START_LIMIT;
    let stream = loop {
        if let Ok(stream) = tokio::net::UnixStream::connect(&fixture.socket).await {
            if std::fs::read_to_string(&fixture.pid_file)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
                == Some(pid)
            {
                break stream;
            }
        }
        assert!(
            launcher.0.try_wait().unwrap().is_none(),
            "launcher exited before bind: {}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "launcher did not bind: {}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let marker = assert_supervisor_marker(&fixture.marker, pid, 10);
    assert_eq!(
        stream.peer_cred().unwrap().pid(),
        Some(i32::try_from(pid).unwrap())
    );
    assert_eq!(
        std::fs::read_to_string(&fixture.pid_file)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap(),
        pid
    );
    drop(stream);
    // Even a clean daemon stop leaves marker cleanup to the deployment's
    // release command. SIGKILL alone would not exercise daemon cleanup code.
    // SAFETY: this is the unreaped child returned by our own spawn above.
    assert_eq!(
        unsafe { libc::kill(i32::try_from(pid).unwrap(), libc::SIGTERM) },
        0
    );
    let stop_deadline = tokio::time::Instant::now() + START_LIMIT;
    loop {
        if let Some(status) = launcher.0.try_wait().unwrap() {
            assert!(status.success(), "daemon did not stop cleanly: {status}");
            break;
        }
        assert!(
            tokio::time::Instant::now() < stop_deadline,
            "daemon did not drain: {}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !fixture.socket.exists(),
        "clean stop must remove its socket"
    );
    assert!(
        !fixture.pid_file.exists(),
        "clean stop must remove its pid file"
    );
    assert_eq!(std::fs::read_to_string(&fixture.marker).unwrap(), marker);
    let (status, log) = fixture.release(LABEL);
    assert!(status.success(), "release failed: {log}");
    assert!(!fixture.marker.exists());
}

// MUST-FAIL: leaving a same-label marker after CONFIG refusal blocks the next
// unmanaged client until the suppression bound, even though no job will restart.
#[test]
fn supervisor_config_refusal_releases_own_marker_without_opening_a_database() {
    for contents in [None, Some("not valid [toml")] {
        let fixture = Fixture::new();
        let bad_config = fixture.root.path().join("bad.toml");
        if let Some(contents) = contents {
            std::fs::write(&bad_config, contents).unwrap();
        }
        std::fs::write(&fixture.marker, format!("{LABEL}\n1\n10\n")).unwrap();
        let (status, log) =
            fixture.completed(&mut fixture.launch_command(&bad_config), "config.log");
        assert!(status.success(), "CONFIG refusal must exit zero: {log}");
        assert!(log.contains("CONFIG"), "must disclose refusal: {log}");
        assert!(
            log.contains(bad_config.to_str().unwrap()),
            "wrong config path: {log}"
        );
        let expected = if contents.is_some() {
            "config TOML parse error"
        } else {
            "the explicitly selected config file does not exist"
        };
        assert!(log.contains(expected), "wrong CONFIG refusal: {log}");
        assert!(
            !log.contains("missing field `backend`"),
            "invalid valid-config fixture: {log}"
        );
        assert!(!fixture.marker.exists());
        assert!(!fixture.socket.exists());
        assert!(!fixture.pid_file.exists());
        assert!(!fixture.database.exists());
    }
}

// MUST-FAIL: own-label ownership checks must protect a foreign marker on both
// publication and cleanup, including when the proposed launch has bad config.
#[test]
fn supervisor_foreign_marker_survives_launch_and_release_refusals() {
    let fixture = Fixture::new();
    let foreign = b"another-supervisor\n1\n7\n";
    std::fs::write(&fixture.marker, foreign).unwrap();
    for config in [
        fixture.config.clone(),
        fixture.root.path().join("absent.toml"),
    ] {
        let (_, log) = fixture.completed(&mut fixture.launch_command(&config), "foreign.log");
        assert!(log.contains("another-supervisor"), "{log}");
        assert_eq!(
            std::fs::read(&fixture.marker).unwrap().as_slice(),
            foreign.as_slice()
        );
        assert!(!fixture.socket.exists());
        assert!(!fixture.pid_file.exists());
        assert!(!fixture.database.exists());
    }
    let (status, log) = fixture.release(LABEL);
    assert!(!status.success(), "foreign release must refuse: {log}");
    assert_eq!(
        std::fs::read(&fixture.marker).unwrap().as_slice(),
        foreign.as_slice()
    );
}

fn assert_pack_refusal(fixture: &Fixture, command: &mut Command, expected: &str) {
    let (status, log) = fixture.completed(command, "pack-refusal.log");
    assert!(
        status.success(),
        "pack CONFIG refusal must exit zero: {log}"
    );
    assert!(
        log.contains("CONFIG"),
        "must disclose CONFIG refusal: {log}"
    );
    assert!(log.contains(expected), "wrong pack refusal: {log}");
    assert!(
        !fixture.marker.exists(),
        "pack refusal must release the marker: {log}"
    );
    assert!(!fixture.socket.exists());
    assert!(!fixture.pid_file.exists());
    assert!(
        !fixture.database.exists(),
        "pack refusal must precede database creation: {log}"
    );
}

#[test]
fn supervisor_unknown_cli_pack_refuses_before_publish_and_releases_prior_marker() {
    for prior_marker in [false, true] {
        let fixture = Fixture::new();
        if prior_marker {
            std::fs::write(&fixture.marker, format!("{LABEL}\n1\n10\n")).unwrap();
        }
        assert_pack_refusal(
            &fixture,
            &mut fixture.launch_command_with_packs(&fixture.config, &["not-a-pack"]),
            "unknown pack",
        );
    }
}

#[test]
fn supervisor_duplicate_cli_pack_refuses_before_publish_and_releases_prior_marker() {
    for prior_marker in [false, true] {
        let fixture = Fixture::new();
        if prior_marker {
            std::fs::write(&fixture.marker, format!("{LABEL}\n1\n10\n")).unwrap();
        }
        assert_pack_refusal(
            &fixture,
            &mut fixture.launch_command_with_packs(&fixture.config, &["kg", "kg"]),
            "duplicate pack \"kg\"",
        );
    }
}

#[test]
fn supervisor_unknown_environment_pack_releases_prior_marker() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.marker, format!("{LABEL}\n1\n10\n")).unwrap();
    let mut command = fixture.launch_command_with_packs(&fixture.config, &[]);
    command.env("KHIVE_PACKS", "not-a-pack");
    assert_pack_refusal(&fixture, &mut command, "unknown pack");
}

#[test]
fn supervisor_unknown_config_pack_releases_prior_marker() {
    let fixture = Fixture::new();
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        config.replace("packs = [\"kg\"]", "packs = [\"not-a-pack\"]"),
    )
    .unwrap();
    std::fs::write(&fixture.marker, format!("{LABEL}\n1\n10\n")).unwrap();
    assert_pack_refusal(
        &fixture,
        &mut fixture.launch_command_with_packs(&fixture.config, &[]),
        "unknown pack",
    );
}

#[test]
fn supervisor_missing_pack_dependency_releases_prior_marker() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.marker, format!("{LABEL}\n1\n10\n")).unwrap();
    assert_pack_refusal(
        &fixture,
        &mut fixture.launch_command_with_packs(&fixture.config, &["gtd"]),
        "requires",
    );
}
