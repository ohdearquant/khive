// MUST-FAIL: an accepted but silent socket does not grant permission to
// signal its peer, even though the peer is under the same uid.
#[test]
fn supervisor_silent_same_uid_stub_is_not_signalled() {
    let fixture = Fixture::new();
    let (mut stub, stub_log) =
        fixture.spawn(&mut fixture.stub_command("silent"), "silent-stub.log");
    wait_stub_socket(&fixture, &mut stub, &stub_log);
    let (status, log) = fixture.completed(
        &mut fixture.launch_command(&fixture.config),
        "silent-launch.log",
    );
    assert!(!status.success(), "silent socket must refuse: {log}");
    assert!(
        log.contains("incumbent is not a khive daemon"),
        "wrong refusal: {log}"
    );
    assert!(
        log.contains(&stub.0.id().to_string()),
        "missing peer pid: {log}"
    );
    assert!(log.contains("uid="), "missing uid: {log}");
    assert!(log.contains("waited="), "missing elapsed time: {log}");
    assert!(
        fixture.marker.exists(),
        "failed handover must retain marker"
    );
    assert!(
        stub.0.try_wait().unwrap().is_none(),
        "silent stub was signalled"
    );

    // Preserve an own-label declaration if its socket accepts but cannot
    // complete an identity response right now.
    let own = Fixture::new();
    let (mut own_stub, own_log) = own.spawn(&mut own.stub_command("silent"), "own-silent-stub.log");
    wait_stub_socket(&own, &mut own_stub, &own_log);
    let marker = format!("{LABEL}\n{}\n1\n", own_stub.0.id());
    std::fs::write(&own.marker, &marker).unwrap();
    let (status, log) = own.completed(
        &mut own.launch_command(&own.config),
        "own-silent-launch.log",
    );
    assert!(
        !status.success(),
        "unidentified own socket must refuse: {log}"
    );
    assert!(
        log.contains("preserving marker"),
        "wrong own-job refusal: {log}"
    );
    assert_eq!(std::fs::read_to_string(&own.marker).unwrap(), marker);
    assert!(
        own_stub.0.try_wait().unwrap().is_none(),
        "unidentified own stub was signalled"
    );
}

// MUST-FAIL: the PID-file owner is not the socket holder and cannot authorize
// handover of, or signalling to, that unrelated locked decoy.
#[test]
fn supervisor_pid_file_decoy_never_selects_signal_target() {
    let fixture = Fixture::new();
    let (mut unrelated, unrelated_log) =
        fixture.spawn(&mut fixture.locked_pid_command(), "unrelated-pid.log");
    wait_locked_pid_child(&fixture, &mut unrelated, &unrelated_log);
    let (status, log) = fixture.completed(
        &mut fixture.launch_command(&fixture.config),
        "pid-only-launch.log",
    );
    assert!(
        !status.success(),
        "daemon should conservatively refuse live PID-file owner: {log}"
    );
    assert!(
        log.contains("live process owns the PID file"),
        "launcher did not reach daemon exec: {log}"
    );
    assert!(
        unrelated.0.try_wait().unwrap().is_none(),
        "unrelated PID-file process was signalled"
    );
    assert_eq!(
        std::fs::read_to_string(&fixture.pid_file).unwrap(),
        format!("{}\n", unrelated.0.id())
    );

    // Separate poison subcase: a probe-answering daemon-like stub owns the
    // socket while the PID file still names an unrelated live process.
    let poison = Fixture::new();
    let (mut decoy, decoy_log) = poison.spawn(&mut poison.locked_pid_command(), "decoy-pid.log");
    wait_locked_pid_child(&poison, &mut decoy, &decoy_log);
    let (mut holder, holder_log) =
        poison.spawn(&mut poison.stub_command("answer"), "answer-stub.log");
    wait_stub_socket(&poison, &mut holder, &holder_log);
    let (status, log) = poison.completed(
        &mut poison.launch_command_options(&poison.config, &["kg"], "1", true),
        "poison-launch.log",
    );
    assert!(
        !status.success(),
        "unacknowledged holder must block exec: {log}"
    );
    assert!(
        log.contains("socket-bound handover not acknowledged"),
        "holder did not refuse handover before daemon exec: {log}"
    );
    assert!(
        decoy.0.try_wait().unwrap().is_none(),
        "decoy PID-file process was signalled"
    );
    assert_eq!(
        std::fs::read_to_string(&poison.pid_file).unwrap(),
        format!("{}\n", decoy.0.id())
    );
    assert!(holder.0.try_wait().unwrap().is_none(), "stub was signalled");
}

// MUST-FAIL: a daemon-like socket response without the handover ack cannot
// authorize signalling the captured numeric PID.
#[tokio::test]
async fn supervisor_unacknowledged_holder_is_never_signalled() {
    let fixture = Fixture::new();
    let (mut holder, holder_log) =
        fixture.spawn(&mut fixture.stub_command("answer"), "answer-stub.log");
    wait_stub_socket(&fixture, &mut holder, &holder_log);
    let (status, log) = fixture.completed(
        &mut fixture.launch_command_options(&fixture.config, &["kg"], "1", true),
        "no-ack-launch.log",
    );
    assert!(
        !status.success(),
        "unacknowledged holder must block exec: {log}"
    );
    assert!(
        log.contains("socket-bound handover not acknowledged"),
        "wrong refusal: {log}"
    );
    assert!(
        log.contains(&holder.0.id().to_string()),
        "missing peer pid: {log}"
    );
    assert!(
        fixture.marker.exists(),
        "failed handover must retain marker"
    );
    assert!(
        holder.0.try_wait().unwrap().is_none(),
        "holder was signalled"
    );
    assert_eq!(socket_holder_pid(&fixture).await, Some(holder.0.id()));
}

#[test]
fn supervisor_exit_between_probe_and_ack_never_signals_captured_pid() {
    // Test the live disconnect first: the numeric-signal mutant fails before
    // reaching the case whose PID has already been released by the kernel.
    // A disconnecting peer stays alive so an unsafe numeric-PID signal on
    // the same failed-ACK path has an observable, deterministic target.
    let disconnected = Fixture::new();
    let (mut holder, holder_log) = disconnected.spawn(
        &mut disconnected.stub_command("disconnect-before-ack"),
        "disconnect-before-ack-stub.log",
    );
    wait_stub_socket(&disconnected, &mut holder, &holder_log);
    let (status, log) = disconnected.completed(
        &mut disconnected.launch_command_options(&disconnected.config, &["kg"], "1", true),
        "disconnect-before-ack-launch.log",
    );
    assert!(!status.success(), "lost ACK must block exec: {log}");
    assert!(
        log.contains("incumbent is not a khive daemon"),
        "lost ACK should refuse as an unidentified holder: {log}"
    );
    assert!(
        holder.0.try_wait().unwrap().is_none(),
        "a failed ACK signalled the captured peer PID"
    );

    let exited = Fixture::new();
    let (mut exiting_holder, exiting_log) = exited.spawn(
        &mut exited.stub_command("exit-before-ack"),
        "exit-before-ack-stub.log",
    );
    wait_stub_socket(&exited, &mut exiting_holder, &exiting_log);
    let (status, log) = exited.completed(
        &mut exited.launch_command_options(&exited.config, &["kg"], "1", true),
        "exit-before-ack-launch.log",
    );
    assert!(!status.success(), "lost ACK must block exec: {log}");
    assert!(
        log.contains("incumbent is not a khive daemon"),
        "lost ACK should refuse as an unidentified holder: {log}"
    );
    assert!(exited.marker.exists(), "failed handover retains its marker");
    assert!(
        exiting_holder.0.try_wait().unwrap().is_some(),
        "fixture holder did not exit before ACK"
    );
}

#[tokio::test]
async fn supervisor_acknowledged_handover_still_bounds_nonexiting_holder() {
    let fixture = Fixture::new();
    let (mut holder, holder_log) =
        fixture.spawn(&mut fixture.stub_command("ack-no-exit"), "ack-stub.log");
    wait_stub_socket(&fixture, &mut holder, &holder_log);
    let started = Instant::now();
    let (status, log) = fixture.completed(
        &mut fixture.launch_command_options(&fixture.config, &["kg"], "1", true),
        "ack-launch.log",
    );
    assert!(
        !status.success(),
        "nonexiting holder must block exec: {log}"
    );
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "wait was not bounded: {log}"
    );
    assert!(log.contains("incumbent did not yield"), "{log}");
    assert!(holder.0.try_wait().unwrap().is_none(), "holder was killed");
    assert_eq!(socket_holder_pid(&fixture).await, Some(holder.0.id()));
}
