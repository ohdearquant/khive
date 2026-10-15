async fn socket_holder_pid(fixture: &Fixture) -> Option<u32> {
    let stream = tokio::net::UnixStream::connect(&fixture.socket)
        .await
        .ok()?;
    stream
        .peer_cred()
        .ok()?
        .pid()
        .and_then(|pid| u32::try_from(pid).ok())
}

async fn wait_for_holder(fixture: &Fixture, expected: Option<u32>) -> u32 {
    let deadline = tokio::time::Instant::now() + START_LIMIT;
    loop {
        if let Some(pid) = socket_holder_pid(fixture).await {
            if expected.is_none_or(|expected| expected == pid)
                && std::fs::read_to_string(&fixture.pid_file)
                    .ok()
                    .and_then(|value| value.trim().parse::<u32>().ok())
                    == Some(pid)
            {
                return pid;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "socket holder did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_pid_gone(pid: u32) {
    let deadline = tokio::time::Instant::now() + START_LIMIT;
    while khive_mcp::daemon::supervisor_pid_is_alive(pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "pid {pid} did not leave"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn wait_child_exit(child: &mut OwnedChild, log_path: &Path) -> (ExitStatus, String) {
    let deadline = Instant::now() + START_LIMIT;
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            return (status, std::fs::read_to_string(log_path).unwrap());
        }
        assert!(
            Instant::now() < deadline,
            "child did not exit: {}",
            std::fs::read_to_string(log_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn is_handover_interrupted_stats(log: &str) -> bool {
    // A client already reading from the unmanaged daemon may receive its
    // cancelled read when the launcher drains that daemon. Accept only the
    // observed one-op stats response, never an arbitrary CLI failure.
    log.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|response| {
            if response["status"] != "partial"
                || response["summary"]["total"] != 1
                || response["summary"]["failed"] != 1
                || response["summary"]["succeeded"] != 0
                || response["summary"]["aborted"] != 0
            {
                return false;
            }
            let Some(results) = response["results"].as_array() else {
                return false;
            };
            if results.len() != 1
                || results[0]["tool"] != "stats"
                || results[0]["ok"] != false
                || results[0]["error"]["kind"] != "runtime_error"
            {
                return false;
            }
            matches!(
                results[0]["error"]["message"].as_str(),
                Some("storage: timeout during count_entities")
                    | Some("storage: timeout during count_notes_in_namespaces")
            )
        })
}

/// This test body also serves as a subprocess-only Unix socket holder. The
/// parent launches the exact test name with an environment-selected mode.
#[test]
fn supervisor_socket_stub_child() {
    let Ok(mode) = std::env::var("KHIVE_SUPERVISOR_STUB_MODE") else {
        return;
    };
    let socket = PathBuf::from(std::env::var_os("KHIVE_SUPERVISOR_STUB_SOCKET").unwrap());
    if mode == "ack-no-exit" {
        // SAFETY: this subprocess is the isolated test stub and intentionally
        // stays alive after claiming to accept handover.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    for incoming in listener.incoming() {
        let Ok(mut stream) = incoming else { continue };
        let mode = mode.clone();
        std::thread::spawn(move || {
            if mode == "silent" {
                std::thread::sleep(Duration::from_secs(60));
                return;
            }
            let mut length = [0_u8; 4];
            if stream.read_exact(&mut length).is_err() {
                return;
            }
            let mut frame = vec![0_u8; u32::from_be_bytes(length) as usize];
            if stream.read_exact(&mut frame).is_err() {
                return;
            }
            let request: serde_json::Value = serde_json::from_slice(&frame).unwrap();
            if request["supervisor_handover"] == true {
                if mode == "exit-before-ack" {
                    std::process::exit(0);
                }
                if mode == "disconnect-before-ack" {
                    return;
                }
            }
            let mut response = serde_json::json!({
                "ok": false,
                "result": null,
                "error": "stub configuration mismatch",
                "namespace_mismatch": false,
                "config_mismatch": true,
                "served_config_id": "stub-config",
                "version_mismatch": false,
                "daemon_protocol_version": khive_runtime::daemon::PROTOCOL_VERSION
            });
            if mode == "ack-no-exit" && request["supervisor_handover"] == true {
                response["supervisor_handover_accepted"] = true.into();
            }
            let response = serde_json::to_vec(&response).unwrap();
            stream
                .write_all(&(response.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&response).unwrap();
        });
    }
}

#[test]
fn supervisor_locked_pid_child() {
    let Some(path) = std::env::var_os("KHIVE_SUPERVISOR_STUB_PID_FILE") else {
        return;
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    use std::os::fd::AsRawFd;
    // SAFETY: this child owns the file descriptor until its parent stops it.
    assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
    writeln!(file, "{}", std::process::id()).unwrap();
    file.flush().unwrap();
    loop {
        std::thread::park();
    }
}

fn wait_locked_pid_child(fixture: &Fixture, child: &mut OwnedChild, log_path: &Path) {
    use std::os::fd::AsRawFd;

    let deadline = Instant::now() + START_LIMIT;
    let expected = format!("{}\n", child.0.id());
    while std::fs::read_to_string(&fixture.pid_file).ok().as_deref() != Some(expected.as_str()) {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "PID lock child exited: {}",
            std::fs::read_to_string(log_path).unwrap_or_default()
        );
        assert!(Instant::now() < deadline, "PID lock child did not publish");
        std::thread::sleep(Duration::from_millis(20));
    }
    let probe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fixture.pid_file)
        .unwrap();
    // SAFETY: this is a nonblocking probe of a file owned by the fixture.
    let rc = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, -1, "PID fixture did not hold its file lock");
    let errno = std::io::Error::last_os_error().raw_os_error();
    assert!(
        errno == Some(libc::EWOULDBLOCK) || errno == Some(libc::EAGAIN),
        "unexpected PID lock probe error: {errno:?}"
    );
}

fn wait_stub_socket(fixture: &Fixture, stub: &mut OwnedChild, log_path: &Path) {
    let deadline = Instant::now() + START_LIMIT;
    while !fixture.socket.exists() {
        assert!(
            stub.0.try_wait().unwrap().is_none(),
            "stub exited: {}",
            std::fs::read_to_string(log_path).unwrap_or_default()
        );
        assert!(Instant::now() < deadline, "stub did not bind");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// MUST-FAIL: removing Fixture::drop's reap leaves the auto-spawned daemon
// alive after the client process and its temporary rendezvous are gone.
#[tokio::test]
async fn fixture_drop_reaps_client_spawned_daemon() {
    let fixture = Fixture::new();
    let (status, log) = fixture.completed(&mut fixture.exec_command(), "reap-client.log");
    assert!(status.success(), "client auto-spawn failed: {log}");
    let pid = wait_for_holder(&fixture, None).await;
    let config = fixture.config.clone();
    let while_alive = fixture_daemon_pids(&config);
    assert_eq!(while_alive, vec![pid], "ps missed the live fixture daemon");

    drop(fixture);
    let after_drop = fixture_daemon_pids(&config);
    println!(
        "fixture daemon ps count: while_alive={} after_drop={}",
        while_alive.len(),
        after_drop.len()
    );
    assert!(
        after_drop.is_empty(),
        "fixture drop left a client-spawned daemon alive: {after_drop:?}"
    );
}

// MUST-FAIL: removing the post-publish handover leaves the client-started
// daemon holding the socket and the supervised job refused by first writer.
#[tokio::test]
async fn supervisor_client_first_handover_and_legacy_control() {
    let fixture = Fixture::new();
    let (status, log) = fixture.completed(&mut fixture.exec_command(), "client-first.log");
    assert!(status.success(), "client auto-spawn failed: {log}");
    let old_pid = wait_for_holder(&fixture, None).await;
    let (mut launcher, launcher_log) = fixture.spawn(
        &mut fixture.launch_command_options(&fixture.config, &["kg"], "10", false),
        "handover.log",
    );
    let job_pid = launcher.0.id();
    assert_ne!(old_pid, job_pid);
    assert_eq!(wait_for_holder(&fixture, Some(job_pid)).await, job_pid);
    wait_pid_gone(old_pid).await;
    assert_supervisor_marker(&fixture.marker, job_pid, 10);
    let (status, log) = fixture.completed(&mut fixture.exec_command(), "client-again.log");
    assert!(
        status.success(),
        "next request was not served by supervised configuration: {log}"
    );
    let (status, log) = fixture.completed(&mut fixture.plan_command(), "client-plan.log");
    assert!(
        status.success(),
        "supervised config did not answer a daemon-only plan: {log}"
    );
    assert_eq!(socket_holder_pid(&fixture).await, Some(job_pid));
    assert!(
        launcher.0.try_wait().unwrap().is_none(),
        "supervised daemon exited: {}",
        std::fs::read_to_string(launcher_log).unwrap_or_default()
    );
    assert!(
        std::fs::read_to_string(&launcher_log)
            .unwrap_or_default()
            .contains("supervisor replaced client-started incumbent"),
        "launcher did not log the replacement"
    );

    // Pre-A2 control in this same test file: publish the declaration, then
    // start the old direct daemon path without the launcher's handover. Both
    // the rendezvous and store are occupied; the socket verdict wins.
    let control = Fixture::new();
    let (status, log) = control.completed(&mut control.exec_command(), "control-client.log");
    assert!(status.success(), "control client auto-spawn failed: {log}");
    let control_holder = wait_for_holder(&control, None).await;
    let store_lock = control.database.with_file_name(".db.khived.lock");
    assert_eq!(
        std::fs::read_to_string(&store_lock).expect("incumbent store claim"),
        control_holder.to_string(),
        "the socket and store claims must both be held for this precedence check"
    );
    let (mut legacy_job, legacy_log) =
        control.spawn(&mut control.direct_daemon_command(), "legacy-job.log");
    let legacy_pid = legacy_job.0.id();
    std::fs::write(&control.marker, format!("{LABEL}\n{legacy_pid}\n10\n")).unwrap();
    let (status, log) = wait_child_exit(&mut legacy_job, &legacy_log);
    assert!(
        !status.success(),
        "pre-handover job unexpectedly served: {log}"
    );
    assert!(
        log.contains("already serving this socket"),
        "wrong first-writer refusal: {log}"
    );
    assert_ne!(socket_holder_pid(&control).await, Some(legacy_pid));
    assert_eq!(socket_holder_pid(&control).await, Some(control_holder));
    // SAFETY: this PID came from the fixture's own socket peer credentials.
    assert_eq!(
        unsafe { libc::kill(i32::try_from(control_holder).unwrap(), libc::SIGTERM) },
        0
    );
    wait_pid_gone(control_holder).await;
}
