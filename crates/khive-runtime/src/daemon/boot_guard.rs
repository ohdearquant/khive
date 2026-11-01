#![cfg(unix)]
use super::*;

/// Run the daemon using a startup lock acquired by the caller *before*
/// building `dispatcher`, so a second process racing to boot (e.g. two
/// `kkernel mcp --daemon` spawns before either has bound its socket) cannot
/// run migrations/FTS DDL concurrently against the same database file.
/// `boot_guard` is only `None` on non-unix targets, where there is no
/// advisory boot lock to hold in the first place; every unix daemon-mode
/// caller passes `Some`.
///
/// The guard is held across cleanup → pid-claim → bind, then dropped. The
/// caller must not still be holding a *different* handle to the same lock
/// file when this function is entered — see the "Deadlock note" on the
/// `_startup_lock` binding below for why that would self-deadlock on `flock`.
#[cfg(unix)]
pub async fn run_daemon_with_boot_guard<D: DaemonDispatch>(
    dispatcher: D,
    boot_guard: Option<std::fs::File>,
) -> anyhow::Result<()> {
    run_daemon_with_boot_guard_inner(
        dispatcher,
        boot_guard,
        false,
        DaemonOptions::default(),
        |_| DaemonStartupReport::default(),
    )
    .await
}

/// Run the daemon and start host-owned background work only after the socket
/// is bound, permissions are restricted, and this process owns the PID file.
/// The callback runs once while the startup lock and teardown guard are held;
/// setup failures never invoke it. Work started by the callback must use the
/// daemon shutdown token and tracked-task drain contract.
#[cfg(unix)]
pub async fn run_daemon_with_boot_guard_and_start<D, F>(
    dispatcher: D,
    boot_guard: Option<std::fs::File>,
    start: F,
) -> anyhow::Result<()>
where
    D: DaemonDispatch,
    F: FnOnce(&D) + Send,
{
    run_daemon_with_options_and_boot_guard_and_start(
        dispatcher,
        boot_guard,
        DaemonOptions::default(),
        |dispatcher| {
            start(dispatcher);
            DaemonStartupReport::default()
        },
    )
    .await
}

/// Start with an explicit launch mode and collect the host's startup inventory.
#[cfg(unix)]
pub async fn run_daemon_with_options_and_boot_guard_and_start<D, F>(
    dispatcher: D,
    boot_guard: Option<std::fs::File>,
    options: DaemonOptions,
    start: F,
) -> anyhow::Result<()>
where
    D: DaemonDispatch,
    F: FnOnce(&D) -> DaemonStartupReport + Send,
{
    // This refusal precedes the inner teardown guard. Close the shared domain
    // even when a retained dispatcher/runtime clone still owns its root. Keep
    // the existing process-token policy: only the inner guard cancels it.
    if options.idle_interval.is_zero() {
        if let Some(resources) = dispatcher.background_resources() {
            if let Err(error) = resources.close_with_policy() {
                tracing::warn!(error = %error, "background resource setup refusal deadline failed");
            }
        }
    }
    anyhow::ensure!(
        !options.idle_interval.is_zero(),
        "daemon idle interval must be positive"
    );
    run_daemon_with_boot_guard_inner(dispatcher, boot_guard, false, options, start).await
}

#[cfg(unix)]
pub(super) async fn run_daemon_with_boot_guard_inner<D, F>(
    dispatcher: D,
    boot_guard: Option<std::fs::File>,
    allow_same_process_incumbent: bool,
    options: DaemonOptions,
    start: F,
) -> anyhow::Result<()>
where
    D: DaemonDispatch,
    F: FnOnce(&D) -> DaemonStartupReport + Send,
{
    // Cancel on every exit, including setup failure and unwinding from the
    // post-ownership startup callback. The guard precedes all fallible work
    // so even a process that never becomes the daemon relinquishes its
    // process-lifetime shutdown token; restarting requires exec.
    struct ComponentTeardown {
        resources: Option<crate::resource_admission::BackgroundDomainControl>,
    }
    impl Drop for ComponentTeardown {
        fn drop(&mut self) {
            if let Some(resources) = &self.resources {
                if let Err(error) = resources.close_with_policy() {
                    tracing::warn!(error = %error, "background resource teardown deadline failed");
                }
            }
            daemon_shutdown_token().cancel();
        }
    }
    let background_resources = dispatcher.background_resources();
    let _component_teardown = ComponentTeardown {
        resources: background_resources.clone(),
    };

    // Placed after the teardown guard so this refusal keeps the ADR-119
    // contract every other pre-bind error path has, and before the paths are
    // resolved so a split rendezvous never reaches cleanup/bind/pid-write.
    ensure_rendezvous_overrides_paired()?;

    if super::lifecycle::path_override(PID_PATH_ENV).is_none()
        && super::lifecycle::path_override(SOCKET_PATH_ENV).is_none()
    {
        super::run_daemon::prepare_default_rendezvous_directory()?;
        #[cfg(test)]
        if let Some(hook) = super::run_daemon::DEFAULT_RENDEZVOUS_PREBIND_HOOK
            .lock()
            .expect("default rendezvous test hook")
            .take()
        {
            hook();
        }
    }

    let sock = socket_path();
    let pid_file = pid_path();
    let socket_parent = sock.parent();
    let pid_parent = pid_file.parent();

    if let Some(parent) = socket_parent {
        std::fs::create_dir_all(parent)?;
        ensure_socket_dir_is_trusted(parent)?;
    }
    // Identical parent paths traverse the same components, so the socket
    // check above also vets the PID-file parent. Aliased paths are checked
    // independently because each original path is traversed by file access.
    if pid_parent != socket_parent {
        ensure_pid_file_dir_is_trusted(&pid_file)?;
    }

    // Hold the startup lock across cleanup → pid-claim → bind so a concurrent
    // client's kill_and_respawn (which also holds this lock) cannot remove the
    // rendezvous paths during setup. The PID file's own lock is retained after
    // this shared startup lock is released, including while the listener drains
    // during shutdown.
    //
    // Deadlock note: the client holds this lock only during kill+spawn and
    // releases it before the spawned daemon process starts (the lock guard is
    // dropped when kill_and_respawn returns, before the readiness probe loop).
    // The daemon holds exactly one handle to this lock for its whole boot
    // sequence (received as `boot_guard`, extended from before `dispatcher`
    // was constructed) — never a second, independently-acquired handle in the
    // same process, which would self-deadlock on `flock`.
    let _startup_lock = boot_guard;

    // A second daemon must refuse loudly rather than exit successfully while
    // another daemon owns this rendezvous. Only `Stale` lets the caller proceed.
    match cleanup_stale_daemon(
        &sock,
        &pid_file,
        allow_same_process_incumbent,
        dispatcher.config_id(),
    )
    .await
    {
        Incumbent::Serving(incumbent_pid) => {
            tracing::error!(
                pid = incumbent_pid,
                socket = ?sock,
                "refusing to start: a khived instance is already serving this socket"
            );
            anyhow::bail!(
                "refusing to start: khived is already running as pid {incumbent_pid}, \
                 serving socket {}. Stop that instance first if you intend to replace it.",
                sock.display()
            );
        }
        Incumbent::Live(incumbent_pid) => {
            tracing::error!(
                pid = incumbent_pid,
                socket = ?sock,
                "refusing to start: a live process owns the PID file but no khived answered"
            );
            anyhow::bail!(
                "refusing to start: pid {incumbent_pid} owns the daemon PID file and is alive, \
                 but nothing answered the khived protocol on {}. It may be draining. Nothing \
                 was removed; stop that process first if you intend to replace it.",
                sock.display()
            );
        }
        Incumbent::Unknown => {
            anyhow::bail!(
                "refusing to start: daemon rendezvous at {} could be live or could not be \
                 inspected safely. Nothing was removed after the uncertainty was detected.",
                sock.display()
            );
        }
        Incumbent::Stale => {}
    }

    // Register the sender recorder before the shutdown streams below. The
    // signal registry runs a signal's actions in registration order, so the
    // recorder stores the sender before tokio's action wakes the shutdown
    // path; registered after it, the exit record can say sender_pid=unknown.
    if let Err(error) = exit_record::capture_signal_senders() {
        tracing::warn!(target: "khive.boot", error = %error, "signal sender capture unavailable; the exit record will say sender_pid=unknown");
    }
    // Install signal streams before publishing either rendezvous file. A
    // supervisor may stop us as soon as connect/pid checks succeed, before
    // the accept loop or shutdown future has been polled. Keep these streams
    // alive so a signal during the rest of startup reaches normal cleanup.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    let pid_file_guard = match write_pid_file_exclusive(&pid_file) {
        Ok(guard) => guard,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // A PID file appeared between cleanup and our claim. Never touch
            // the winner's files; defer only if it already answers as khived.
            if pid_file_names_a_reachable_daemon(
                &pid_file,
                &sock,
                allow_same_process_incumbent,
                dispatcher.config_id(),
            )
            .await
            {
                tracing::info!(
                    "a replacement khived already claimed the pid/socket rendezvous; exiting"
                );
                return Ok(());
            }
            anyhow::bail!(
                "failed to claim daemon pid file at {pid_file:?}: it already exists \
                 and does not name a reachable daemon"
            );
        }
        Err(e) => return Err(e.into()),
    };
    // After the claim, never before: only the PID file's owner writes the record.
    let exit_record = exit_record::ExitRecord::claim(&pid_file);

    let listener = match UnixListener::bind(&sock) {
        Ok(listener) => listener,
        Err(e) => {
            exit_record.exiting(exit_record::ExitReason::Error);
            remove_pid_file_if_owned(&pid_file, &pid_file_guard);
            return Err(e.into());
        }
    };
    // Fail closed, same reason as the directory above. If this chmod fails the
    // socket is world-reachable in a way the accepted design never covered, so
    // the bound listener is dropped and the entry removed rather than served.
    if let Err(e) = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)) {
        drop(listener);
        let _ = std::fs::remove_file(&sock);
        exit_record.exiting(exit_record::ExitReason::Error);
        remove_pid_file_if_owned(&pid_file, &pid_file_guard);
        return Err(anyhow::anyhow!(
            "refusing to start: cannot chmod 0600 {}: {e}. The daemon socket must be owner-only \
             — it is half of the single-principal guarantee this daemon enforces.",
            sock.display()
        ));
    }
    // Captured while still holding the startup lock, immediately after
    // bind, so shutdown cleanup can later prove "this is still the same socket
    // I bound" rather than trusting the path alone.
    let bound_identity = socket_identity(&sock);

    let lifecycle = Arc::new(DaemonLifecycle::new(options, start(&dispatcher)));

    // Release the shared startup lock now that the listener is bound. The
    // locked PID file continues to identify this daemon through shutdown.
    drop(_startup_lock);
    tracing::info!(
        socket = ?sock,
        pid = std::process::id(),
        source_revision = crate::BUILD_INFO.source_revision,
        build_time = crate::BUILD_INFO.build_time,
        "khived listening"
    );

    {
        let warm = dispatcher.clone();
        track_named_background_task("daemon_warmup", async move {
            warm.warm_all().await;
        });
    }

    // The checkpoint task's own strong-count-based exit is unreachable
    // whenever `event_store_for_checkpoint()` returns `Some` (the ordinary
    // production shape), because the `SqlEventStore` it wraps retains its
    // own clone of the same pool. An explicit watch channel replaces that
    // mechanism: the sender is held for the remainder of this function's
    // scope and signalled as the first action once shutdown is observed,
    // below.
    let (checkpoint_shutdown_tx, checkpoint_shutdown_rx) = tokio::sync::watch::channel(());
    // ADR-091 Amendment 3: one checkpoint task per file-backed backend the
    // dispatcher wired — the primary pool plus every entry
    // `secondary_pools_for_checkpoint` returns — sharing this one shutdown
    // channel (the sender broadcasts to every receiver clone), so the single
    // send below stops every spawned task before `drain()`.
    let checkpoint_tasks = checkpoint_task_specs(
        dispatcher.pool_for_checkpoint(),
        dispatcher.secondary_pools_for_checkpoint(),
        dispatcher.event_store_for_checkpoint(),
        dispatcher.namespace().to_string(),
    );
    if !checkpoint_tasks.is_empty() {
        let cfg = CheckpointConfig::from_env();
        let checkpoint_task_count = checkpoint_tasks.len();
        for task in checkpoint_tasks {
            track_named_background_task(
                "wal_checkpoint",
                run_checkpoint_task(
                    task.pool,
                    cfg.clone(),
                    task.lifecycle_owner,
                    checkpoint_shutdown_rx.clone(),
                    task.is_main,
                ),
            );
        }
        tracing::info!(checkpoint_task_count, "WAL checkpoint task(s) started");
    }

    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connection_tasks = Arc::new(std::sync::Mutex::new(
        Vec::<tokio::task::JoinHandle<()>>::new(),
    ));
    let (request_shutdown_tx, request_shutdown_rx) = tokio::sync::watch::channel(false);

    let shutdown = async {
        let signal = tokio::select! {
            _ = sigterm.recv() => "SIGTERM",
            _ = sigint.recv() => "SIGINT",
        };
        tracing::info!("received {signal}");
        // Tokio retains its process-wide handlers after the streams are dropped.
        // This daemon cannot restart without exec; a repeat signal must terminate
        // even if shutdown is blocked in synchronous recovery-lock acquisition.
        for signal in [libc::SIGTERM, libc::SIGINT] {
            // SAFETY: setting SIG_DFL for these valid signals needs no handler
            // pointer or shared Rust state and applies to the whole process.
            if unsafe { libc::signal(signal, libc::SIG_DFL) } == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok::<&'static str, std::io::Error>(signal)
    };
    tokio::pin!(shutdown);
    lifecycle.ready();

    // SAFETY: `geteuid` is always successful and takes no arguments.
    let daemon_euid = unsafe { libc::geteuid() } as u32;

    let reason = tokio::select! {
        _ = async {
            let mut accept_error_backoff = None;
            let mut last_accept_error_log: Option<std::time::Instant> = None;
            loop {
                match listener.accept().await {
                    Ok((mut stream, _)) => {
                        let initial_frame_deadline =
                            tokio::time::Instant::now() + INITIAL_FRAME_READ_TIMEOUT;
                        accept_error_backoff = None;
                        last_accept_error_log = None;
                        // Refuse a foreign uid before any frame is read.
                        // Fails CLOSED: an error reading peer credentials is
                        // "cannot prove same-uid", which is the same answer as
                        // "is not same-uid" — never a pass.
                        //
                        // Scope: this is a same-UID check, which is strictly
                        // weaker than same-PRINCIPAL. Several distinct actors
                        // running under one uid all pass here, so this does not
                        // by itself establish that the daemon serves a single
                        // principal, and nothing downstream refuses or degrades
                        // on observing more than one. Do not describe it as a
                        // multi-principal guard — it bounds the process boundary,
                        // not the identity one.
                        match peer_uid(&stream) {
                            Ok(peer) if uid_is_permitted(peer, daemon_euid) => {}
                            Ok(peer) => {
                                tracing::error!(
                                    peer_uid = peer,
                                    daemon_euid,
                                    "refusing connection from a foreign uid: this daemon accepts \
                                     only peers running as its own uid"
                                );
                                drop(stream);
                                continue;
                            }
                            Err(e) => {
                                tracing::error!(
                                    error = %e,
                                    "refusing connection: cannot read peer credentials, so \
                                     same-uid cannot be proven"
                                );
                                drop(stream);
                                continue;
                            }
                        }
                        // One permit per connection, taken before the task is
                        // spawned. Past the cap the peer is answered with a busy
                        // error and the stream is dropped; connections already
                        // admitted keep their permits and are not affected.
                        let Some(permit) = admit_or_refuse_busy(
                            &lifecycle.connections,
                            &mut stream,
                            dispatcher.config_id(),
                        )
                        .await
                        else {
                            continue;
                        };
                        // Keep the acceptance-time deadline across the
                        // credential check and connection-task scheduling.
                        let d = dispatcher.clone();
                        let shutdown = request_shutdown_rx.clone();
                        let lifecycle = Arc::clone(&lifecycle);
                        let handle = spawn_connection_task(Arc::clone(&active), async move {
                            // Released when the handler ends, whether it returns,
                            // panics or is aborted at shutdown.
                            let _permit = permit;
                            handle_conn_with_lifecycle(
                                stream,
                                d,
                                Some(shutdown),
                                initial_frame_deadline,
                                Some(lifecycle),
                            )
                            .await;
                        });
                        let mut tasks = connection_tasks
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        tasks.retain(|task| !task.is_finished());
                        tasks.push(handle);
                    }
                    Err(e) => {
                        let delay = next_accept_error_backoff(accept_error_backoff);
                        accept_error_backoff = Some(delay);
                        let capacity_exhausted = matches!(
                            e.raw_os_error(),
                            Some(libc::EMFILE) | Some(libc::ENFILE)
                        );
                        if last_accept_error_log.is_none_or(|last| {
                            last.elapsed() >= std::time::Duration::from_secs(30)
                        }) {
                            tracing::error!(
                                error = %e,
                                capacity_exhausted,
                                retry_ms = delay.as_millis(),
                                "daemon accept failed; retrying with bounded backoff"
                            );
                            last_accept_error_log = Some(std::time::Instant::now());
                        }
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        } => DaemonShutdownReason::Signal,
        result = &mut shutdown => {
            exit_record.exiting(exit_record::ExitReason::Signal(result?));
            DaemonShutdownReason::Signal
        },
        _ = wait_for_idle(&dispatcher, &lifecycle) => {
            exit_record.exiting(exit_record::ExitReason::Idle);
            DaemonShutdownReason::Idle
        },
    };

    lifecycle.draining(reason);

    // A listening backlog is not admitted work. Close it before draining so
    // new clients cannot finish writing to a socket nobody will accept.
    drop(listener);

    // Signal the checkpoint task to exit before draining, so `drain()`
    // actually waits on it via `track_background_task` rather than the
    // task outliving the drain window (or the process) unsignalled.
    let _ = checkpoint_shutdown_tx.send(());

    // Per-run signal: read scopes stop promptly, admitted writes ignore it and
    // retain the rest of the configured drain window to commit or roll back.
    if reason == DaemonShutdownReason::Signal {
        let _ = request_shutdown_tx.send(true);
    }

    // Same ordering contract for ADR-119 daemon components: cancel before
    // drain, so each component's supervisor (itself a tracked task) can run
    // its bounded shutdown inside the drain wait.
    let drain_budget = drain_timeout();
    let cleanup = crate::resource_admission::CleanupPolicy::new(drain_budget);
    let initial_deadline = cleanup.deadline_from(std::time::Instant::now());
    let mut resource_deadline = match initial_deadline {
        Ok(deadline) => deadline,
        Err(error) => {
            tracing::warn!(error = %error, "background resource shutdown deadline overflowed");
            std::time::Instant::now()
        }
    };
    if let Some(resources) = &background_resources {
        resources.close(resource_deadline);
    }
    daemon_shutdown_token().cancel();

    let mut signal_shutdown = reason == DaemonShutdownReason::Signal;
    let drained = if reason == DaemonShutdownReason::Idle {
        tokio::select! {
            _ = drain_for_idle(&active, drain_budget) => true,
            result = &mut shutdown => {
                exit_record.exiting(exit_record::ExitReason::Signal(result?));
                lifecycle.draining(DaemonShutdownReason::Signal);
                signal_shutdown = true;
                let _ = request_shutdown_tx.send(true);
                resource_deadline = cleanup.deadline_from(std::time::Instant::now())
                    .unwrap_or_else(|error| {
                        tracing::warn!(error = %error, "background resource signal deadline overflowed");
                        std::time::Instant::now()
                    });
                if let Some(resources) = &background_resources {
                    resources.close(resource_deadline);
                }
                drain_with_timeout(
                    &active,
                    resource_deadline.saturating_duration_since(std::time::Instant::now()),
                ).await
            }
        }
    } else {
        drain_with_timeout(
            &active,
            resource_deadline.saturating_duration_since(std::time::Instant::now()),
        )
        .await
    };

    if let Some(resources) = &background_resources {
        // Idle drain above has waited for real tracked work without a cutoff.
        // Observe infrastructure exit without extending its expired cleanup
        // deadline. Signal shutdown retains its one original shared budget.
        let exit = if signal_shutdown {
            resources.shutdown(resource_deadline).await
        } else {
            let observation_deadline = cleanup
                .deadline_from(std::time::Instant::now())
                .unwrap_or(resource_deadline);
            resources.observe_exit_until(observation_deadline).await
        };
        match exit {
            Ok(report) if report.workers_exited && report.coordinator_joined => {}
            Ok(report) => tracing::warn!(
                outstanding_jobs = report.outstanding_jobs,
                incomplete_finalizations = report.incomplete_finalizations,
                workers_exited = report.workers_exited,
                coordinator_joined = report.coordinator_joined,
                "background resource shutdown remains incomplete",
            ),
            Err(error) => tracing::warn!(error = %error, "background resource shutdown failed"),
        }
    }
    let tasks = {
        let mut retained = connection_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *retained)
    };
    finish_connection_tasks(tasks, drained).await;

    // A concurrent client's `kill_and_respawn` may have already decided
    // this daemon looked stale, killed it, and spawned a replacement that
    // bound the same socket/PID paths while this daemon was draining above.
    // Reacquire the recovery lock (the same one that serializes startup) and
    // only unlink if the PID file still names this process AND the socket at
    // `sock` is still the exact one this daemon bound — otherwise a
    // replacement daemon owns those paths now and unlinking would delete its
    // live socket/PID out from under it.
    match acquire_recovery_lock() {
        Some(_shutdown_lock) => {
            shutdown_cleanup_if_owned(&sock, &pid_file, bound_identity);
        }
        None => {
            tracing::warn!(
                "could not acquire recovery lock for shutdown cleanup; \
                 skipping unlink to avoid deleting a replacement daemon's paths"
            );
        }
    }
    lifecycle.stopped();
    tracing::info!("khived stopped");
    Ok(())
}

/// Remove `sock`/`pid_file` only if they still belong to this process: the PID
/// file must name `std::process::id()` AND the socket currently at `sock` must
/// still be the exact one identified by `bound_identity` (dev/ino, not path).
///
/// Returns `true` if cleanup ran, `false` if it was skipped because a
/// replacement daemon already owns those paths. The caller must hold
/// the recovery lock across this call — the same lock daemon startup holds
/// across cleanup+bind+pid-write — so no replacement can bind between this
/// function's checks and its unlinks.
#[cfg(unix)]
pub(super) fn shutdown_cleanup_if_owned(
    sock: &std::path::Path,
    pid_file: &std::path::Path,
    bound_identity: Option<SocketIdentity>,
) -> bool {
    let pid_is_ours = std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        == Some(std::process::id());
    let socket_is_ours = bound_identity.is_some() && socket_identity(sock) == bound_identity;
    if pid_is_ours && socket_is_ours {
        let _ = std::fs::remove_file(sock);
        let _ = std::fs::remove_file(pid_file);
        true
    } else {
        tracing::warn!(
            socket = ?sock,
            pid_file = ?pid_file,
            "skipping shutdown cleanup — a replacement daemon already owns this socket/PID"
        );
        false
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod background_setup_refusal_tests {
    use super::*;
    use crate::resource_admission::{
        BackgroundRoot, CleanupPolicy, ResourceError, TrackedJob, WorkClass, WorkClassLimits,
    };
    use std::time::{Duration, Instant};

    #[derive(Clone)]
    struct RetainedResourceDispatcher {
        root: BackgroundRoot,
    }

    #[async_trait::async_trait]
    impl DaemonDispatch for RetainedResourceDispatcher {
        fn background_resources(
            &self,
        ) -> Option<crate::resource_admission::BackgroundDomainControl> {
            Some(self.root.control())
        }
        fn plan(&self, _ops: &str) -> String {
            panic!("setup refusal must not plan")
        }
        async fn dispatch(
            &self,
            _ops: String,
            _presentation: Option<String>,
            _presentation_per_op: Option<Vec<Option<String>>>,
            _format: Option<String>,
            _format_per_op: Option<Vec<Option<String>>>,
            _from_wire: bool,
            _identity: Option<crate::RequestIdentity>,
        ) -> Result<String, String> {
            panic!("setup refusal must not dispatch")
        }
        async fn warm_all(&self) {
            panic!("setup refusal must not warm")
        }
        fn namespace(&self) -> &str {
            "resource-setup-refusal-test"
        }
        fn config_id(&self) -> &str {
            "resource-setup-refusal-test"
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn zero_idle_refusal_closes_shared_domain_despite_retained_dispatcher_clone() {
        // Bound the synchronous constructor receipt as well as async waits.
        // The detached fixture initializer returns/drops its root if this test
        // receiver times out; this is not a production startup-timeout claim.
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let initializer = std::thread::spawn(move || {
            let result = BackgroundRoot::start(
                WorkClassLimits::validate(2, 3).unwrap(),
                CleanupPolicy::new(Duration::from_secs(1)),
            );
            let _ = ready_tx.send(result);
        });
        drop(initializer);
        let root = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("domain fixture startup timed out")
            .expect("domain fixture startup failed");
        let dispatcher = RetainedResourceDispatcher { root };
        let retained = dispatcher.clone();
        let control = retained.root.control();
        assert!(!control.is_closed());
        let process_was_cancelled = daemon_shutdown_token().is_cancelled();
        let options = DaemonOptions {
            idle_interval: Duration::ZERO,
            ..Default::default()
        };
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            run_daemon_with_options_and_boot_guard_and_start(dispatcher, None, options, |_| {
                panic!("invalid setup must not invoke startup callback")
            }),
        )
        .await
        .expect("setup refusal timed out")
        .expect_err("zero interval must refuse");
        assert_eq!(error.to_string(), "daemon idle interval must be positive");
        assert!(
            control.is_closed(),
            "retained external root must not leave admission open"
        );
        assert_eq!(
            daemon_shutdown_token().is_cancelled(),
            process_was_cancelled,
            "outer options refusal must preserve existing process-token policy"
        );
        let tracking = TrackedJob::register("resource_test_retained_setup_clone");
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_secs(5),
                retained.root.handle().admit(WorkClass::Warm, &tracking)
            )
            .await
            .expect("closed admission did not settle"),
            Err(ResourceError::Closed)
        ));
        drop(tracking);
        let report = tokio::time::timeout(
            Duration::from_secs(5),
            control.observe_exit_until(Instant::now().checked_add(Duration::from_secs(4)).unwrap()),
        )
        .await
        .expect("exit observation timed out")
        .expect("exit observation failed");
        assert!(report.workers_exited && report.coordinator_joined);
        assert_eq!(report.outstanding_jobs, 0);
        drop(retained);
    }
}
