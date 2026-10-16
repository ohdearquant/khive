// ADR-049 Amendment 11: probes keep observing the same process but cannot
// extend demand lifetime. The persistent process is a matched control.
async fn lifecycle_frame(
    fixture: &Fixture,
    config_id: &str,
    metrics: bool,
) -> Option<khive_runtime::daemon::DaemonResponseFrame> {
    use khive_runtime::daemon::{read_frame, write_frame, DaemonRequestFrame, PROTOCOL_VERSION};
    let mut stream = tokio::net::UnixStream::connect(&fixture.socket)
        .await
        .ok()?;
    let request = DaemonRequestFrame {
        metrics_only: metrics,
        probe_only: !metrics,
        protocol_version: PROTOCOL_VERSION,
        config_id: config_id.to_owned(),
        ..Default::default()
    };
    write_frame(&mut stream, &serde_json::to_vec(&request).unwrap())
        .await
        .ok()?;
    let bytes = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut stream))
        .await
        .ok()?
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn wait_lifecycle(
    fixture: &Fixture,
    child: &mut OwnedChild,
) -> khive_runtime::daemon::DaemonResponseFrame {
    let deadline = tokio::time::Instant::now() + START_LIMIT;
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "daemon exited before readiness"
        );
        if let Some(response) = lifecycle_frame(fixture, "", true).await {
            assert!(response.ok);
            assert!(response.metrics.as_ref().unwrap().lifecycle.is_some());
            return response;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "lifecycle diagnostics never became ready"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn demand_idle_retirement_and_persistent_probe_control() {
    use khive_runtime::daemon::DaemonLifetime;
    let demand = Fixture::new();
    let persistent = Fixture::new();
    let (mut demand_child, demand_log) = demand.spawn(
        demand.direct_daemon_command().args([
            "--lifetime",
            "demand",
            "--idle-timeout-secs",
            "1",
            "--no-embed",
        ]),
        "demand-idle.log",
    );
    let (mut persistent_child, _) = persistent.spawn(
        persistent.direct_daemon_command().args([
            "--lifetime",
            "persistent",
            "--idle-timeout-secs",
            "1",
            "--no-embed",
        ]),
        "persistent-idle.log",
    );
    let d = wait_lifecycle(&demand, &mut demand_child).await;
    let p = wait_lifecycle(&persistent, &mut persistent_child).await;
    let d_config = d.served_config_id.unwrap();
    let p_config = p.served_config_id.unwrap();
    assert_eq!(
        d.metrics.unwrap().lifecycle.unwrap().lifetime,
        DaemonLifetime::Demand
    );
    assert_eq!(
        p.metrics.unwrap().lifecycle.unwrap().lifetime,
        DaemonLifetime::Persistent
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut probes = 0;
    loop {
        assert!(
            persistent_child.0.try_wait().unwrap().is_none(),
            "persistent control retired"
        );
        if let Some(status) = demand_child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        // Alternate ordinary protocol probes and metrics diagnostics.
        let _ = lifecycle_frame(&demand, &d_config, probes % 2 == 0).await;
        assert!(lifecycle_frame(&persistent, &p_config, probes % 2 == 0)
            .await
            .is_some());
        probes += 1;
        assert!(
            tokio::time::Instant::now() < deadline,
            "demand daemon must retire despite probe traffic: {}",
            std::fs::read_to_string(&demand_log).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        probes >= 2,
        "the fixture must actually exercise probe traffic"
    );
    assert!(!demand.socket.exists());
    assert!(!demand.pid_file.exists());
    assert!(persistent.socket.exists());
    assert!(persistent.pid_file.exists());
}

#[tokio::test]
async fn demand_second_client_survives_first_exit_and_resets_idle() {
    let fixture = Fixture::new();
    let (mut child, log) = fixture.spawn(
        fixture
            .direct_daemon_command()
            .args(["--lifetime", "demand", "--idle-timeout-secs", "5"]),
        "second-client.log",
    );
    let initial = wait_lifecycle(&fixture, &mut child).await;
    let config = initial.served_config_id.unwrap();
    let generation = initial
        .metrics
        .unwrap()
        .lifecycle
        .unwrap()
        .instance_generation;
    let pid = child.0.id();
    let (first, receipt) = fixture.completed(&mut fixture.exec_command(), "first-client.log");
    assert!(first.success(), "first client failed: {receipt}");
    assert!(
        receipt.contains("execution: answered by daemon;"),
        "first client must dispatch through the daemon, not inline: {receipt}"
    );
    let after_first = lifecycle_frame(&fixture, &config, true).await.unwrap();
    assert!(after_first.ok);
    assert_eq!(
        after_first.served_config_id.as_deref(),
        Some(config.as_str())
    );
    assert_eq!(
        after_first
            .metrics
            .unwrap()
            .lifecycle
            .unwrap()
            .instance_generation,
        generation
    );
    assert_eq!(wait_for_holder(&fixture, Some(pid)).await, pid);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (second, receipt) =
        fixture.completed(&mut fixture.exec_command(), "second-client-receipt.log");
    assert!(second.success(), "second client failed: {receipt}");
    assert!(
        receipt.contains("execution: answered by daemon;"),
        "second client must dispatch through the daemon, not inline: {receipt}"
    );
    let after = lifecycle_frame(&fixture, &config, true).await.unwrap();
    assert!(after.ok);
    assert_eq!(after.served_config_id.as_deref(), Some(config.as_str()));
    assert_eq!(
        after
            .metrics
            .unwrap()
            .lifecycle
            .unwrap()
            .instance_generation,
        generation
    );
    assert_eq!(wait_for_holder(&fixture, Some(pid)).await, pid);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "ordinary cleanup must restart the idle interval"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "idle daemon did not finish: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!fixture.socket.exists());
    assert!(!fixture.pid_file.exists());
}
