// Run explicitly in the hosted gate: cargo test -p kkernel --test
// supervisor_lifecycle supervisor_started_together_twenty_offsets -- --ignored --exact
// Each isolated fixture draws a fresh offset in [0, 1s), including both
// launch-first and client-first orders.
#[tokio::test]
#[ignore]
async fn supervisor_started_together_twenty_offsets() {
    for run in 0_u64..20 {
        let fixture = Fixture::new();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        let offset = Duration::from_millis((nanos.wrapping_mul(791) + run * 53) % 1000);
        let mut launcher_command =
            fixture.launch_command_options(&fixture.config, &["kg"], "10", false);
        let (mut launcher, log_path) = if run % 2 == 0 {
            let (launcher, log_path) = fixture.spawn(&mut launcher_command, "together-launch.log");
            tokio::time::sleep(offset).await;
            let (status, log) =
                fixture.completed(&mut fixture.exec_command(), "together-client.log");
            assert!(status.success(), "run {run}: client failed: {log}");
            (launcher, log_path)
        } else {
            let (mut client, client_log) =
                fixture.spawn(&mut fixture.exec_command(), "together-client.log");
            tokio::time::sleep(offset).await;
            let overlapped = client.0.try_wait().unwrap().is_none();
            let (launcher, log_path) = fixture.spawn(&mut launcher_command, "together-launch.log");
            let (status, log) = wait_child_exit(&mut client, &client_log);
            assert!(
                status.success() || (overlapped && is_handover_interrupted_stats(&log)),
                "run {run}: client failed outside handover: {log}"
            );
            (launcher, log_path)
        };
        let job_pid = launcher.0.id();
        assert_eq!(
            wait_for_holder(&fixture, Some(job_pid)).await,
            job_pid,
            "run {run}"
        );
        let (status, log) =
            fixture.completed(&mut fixture.exec_command(), "together-fresh-client.log");
        assert!(
            status.success(),
            "run {run}: fresh request was not served by supervised daemon: {log}"
        );
        assert_eq!(
            socket_holder_pid(&fixture).await,
            Some(job_pid),
            "run {run}"
        );
        assert!(
            launcher.0.try_wait().unwrap().is_none(),
            "run {run}: launcher exited: {}",
            std::fs::read_to_string(log_path).unwrap_or_default()
        );
    }
}

// MUST-FAIL: removing the own-job guard overwrites the marker and asks the
// daemon already serving this same supervisor job to shut itself down.
#[tokio::test]
async fn supervisor_second_launch_leaves_own_job_and_marker_unchanged() {
    let fixture = Fixture::new();
    let (mut first, log_path) = fixture.spawn(
        &mut fixture.launch_command(&fixture.config),
        "own-first.log",
    );
    let first_pid = first.0.id();
    assert_eq!(wait_for_holder(&fixture, Some(first_pid)).await, first_pid);
    let marker = std::fs::read(&fixture.marker).unwrap();
    let (status, log) = fixture.completed(
        &mut fixture.launch_command(&fixture.config),
        "own-second.log",
    );
    assert!(!status.success(), "duplicate launcher must refuse: {log}");
    assert!(
        log.contains("duplicate supervisor launch"),
        "wrong duplicate refusal: {log}"
    );
    assert_eq!(std::fs::read(&fixture.marker).unwrap(), marker);
    assert_eq!(socket_holder_pid(&fixture).await, Some(first_pid));
    assert!(
        first.0.try_wait().unwrap().is_none(),
        "first job was disturbed: {}",
        std::fs::read_to_string(log_path).unwrap_or_default()
    );
    let bad_config = fixture.root.path().join("invalid.toml");
    std::fs::write(&bad_config, "not valid [toml").unwrap();
    let (status, log) =
        fixture.completed(&mut fixture.launch_command(&bad_config), "own-invalid.log");
    assert!(
        !status.success(),
        "live own job must win before config refusal: {log}"
    );
    assert!(
        log.contains("duplicate supervisor launch"),
        "wrong own-job refusal: {log}"
    );
    assert_eq!(std::fs::read(&fixture.marker).unwrap(), marker);
}

// Simulate PID reuse without depending on the host allocator: the socket
// holder's real PID equals the stale marker PID, but it lacks that launcher's
// incarnation claim. A numeric-only duplicate guard would refuse handover.
#[tokio::test]
async fn supervisor_reused_marker_pid_does_not_adopt_client_daemon() {
    let fixture = Fixture::new();
    let stale_claim = uuid::Uuid::new_v4();
    let (status, log) = fixture.completed(&mut fixture.exec_command(), "reused-client.log");
    assert!(status.success(), "client auto-spawn failed: {log}");
    let client_pid = wait_for_holder(&fixture, None).await;
    std::fs::write(
        &fixture.marker,
        format!("{LABEL}\n{client_pid}\n10\n{stale_claim}\n"),
    )
    .unwrap();
    let (mut launcher, launcher_log) = fixture.spawn(
        &mut fixture.launch_command(&fixture.config),
        "reused-launch.log",
    );
    let job_pid = launcher.0.id();
    assert_eq!(wait_for_holder(&fixture, Some(job_pid)).await, job_pid);
    wait_pid_gone(client_pid).await;
    let marker = assert_supervisor_marker(&fixture.marker, job_pid, 10);
    assert!(!marker.contains(&stale_claim.to_string()));
    assert!(
        launcher.0.try_wait().unwrap().is_none(),
        "launcher failed to hand over reused PID: {}",
        std::fs::read_to_string(launcher_log).unwrap_or_default()
    );
}

#[tokio::test]
async fn supervisor_pid_equal_legacy_marker_refuses_without_claim() {
    let fixture = Fixture::new();
    let (status, log) = fixture.completed(&mut fixture.exec_command(), "legacy-client.log");
    assert!(status.success(), "client auto-spawn failed: {log}");
    let client_pid = wait_for_holder(&fixture, None).await;
    let legacy = format!("{LABEL}\n{client_pid}\n10\n");
    std::fs::write(&fixture.marker, &legacy).unwrap();

    let (status, log) = fixture.completed(
        &mut fixture.launch_command(&fixture.config),
        "legacy-ambiguous-launch.log",
    );
    assert!(!status.success(), "legacy identity must fail closed: {log}");
    assert!(
        log.contains("legacy supervisor marker cannot prove"),
        "{log}"
    );
    assert_eq!(std::fs::read_to_string(&fixture.marker).unwrap(), legacy);
    assert_eq!(socket_holder_pid(&fixture).await, Some(client_pid));
}

#[tokio::test]
async fn supervisor_client_spawn_discards_inherited_claim() {
    let fixture = Fixture::new();
    let stale_claim = uuid::Uuid::new_v4();
    let mut client = fixture.exec_command();
    client.env(
        khive_runtime::daemon::SUPERVISOR_CLAIM_ENV,
        stale_claim.to_string(),
    );
    let (status, log) = fixture.completed(&mut client, "inherited-client.log");
    assert!(status.success(), "client auto-spawn failed: {log}");
    let client_pid = wait_for_holder(&fixture, None).await;
    std::fs::write(
        &fixture.marker,
        format!("{LABEL}\n{client_pid}\n10\n{stale_claim}\n"),
    )
    .unwrap();

    let (mut launcher, launcher_log) = fixture.spawn(
        &mut fixture.launch_command(&fixture.config),
        "inherited-launch.log",
    );
    let job_pid = launcher.0.id();
    assert_eq!(wait_for_holder(&fixture, Some(job_pid)).await, job_pid);
    wait_pid_gone(client_pid).await;
    assert!(
        launcher.0.try_wait().unwrap().is_none(),
        "client daemon inherited the claim: {}",
        std::fs::read_to_string(launcher_log).unwrap_or_default()
    );
}
