// Startup gate for the supervised events daemon (EventsReadiness).
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn readiness_gate_admits_at_once_when_already_answered() {
    let (signal, gate) = events_readiness(PathBuf::from("/tmp/answered.events.sock"), Duration::from_secs(10));
    signal.mark_ready();
    let started = tokio::time::Instant::now();
    assert_eq!(gate.wait().await, Ok(()));
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(gate.is_ready());
    assert!(!signal.awaiting());
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn readiness_gate_holds_a_request_until_the_socket_answers() {
    let (signal, gate) = events_readiness(PathBuf::from("/tmp/late.events.sock"), Duration::from_secs(10));
    let waiter = tokio::spawn({
        let gate = gate.clone();
        async move { gate.wait().await }
    });
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!waiter.is_finished(), "the request must wait while the window is open");
    assert!(signal.awaiting());
    signal.mark_ready();
    assert_eq!(waiter.await.unwrap(), Ok(()));
    assert!(!signal.awaiting());
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn readiness_gate_refuses_after_the_window_and_names_the_socket() {
    let socket = PathBuf::from("/tmp/silent.events.sock");
    let (signal, gate) = events_readiness(socket.clone(), Duration::from_secs(10));
    let started = tokio::time::Instant::now();
    let refused = gate.wait().await.unwrap_err();
    assert_eq!(started.elapsed(), Duration::from_secs(10));
    assert_eq!(refused.reason, EventsNotReadyReason::WindowElapsed);
    assert_eq!(refused.socket, socket);
    assert!(refused.to_string().contains("/tmp/silent.events.sock"), "{refused}");
    assert!(!signal.awaiting(), "a closed window no longer counts as awaiting");

    // The window is measured from supervision start, not per request: a
    // later request is refused at once instead of waiting again.
    let again = tokio::time::Instant::now();
    assert_eq!(gate.wait().await.unwrap_err().reason, EventsNotReadyReason::WindowElapsed);
    assert_eq!(again.elapsed(), Duration::ZERO);

    // A child that binds late (a respawn) reopens the gate.
    signal.mark_ready();
    assert_eq!(gate.wait().await, Ok(()));
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn readiness_gate_refuses_when_supervision_stops_before_an_answer() {
    let (signal, gate) = events_readiness(PathBuf::from("/tmp/stopped.events.sock"), Duration::from_secs(10));
    drop(signal);
    let started = tokio::time::Instant::now();
    let refused = gate.wait().await.unwrap_err();
    assert_eq!(refused.reason, EventsNotReadyReason::SupervisorStopped);
    assert_eq!(started.elapsed(), Duration::ZERO, "a stopped supervisor must not hold the request");
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn readiness_gate_answer_racing_the_deadline_admits() {
    let (signal, gate) = events_readiness(PathBuf::from("/tmp/edge.events.sock"), Duration::from_secs(1));
    tokio::time::sleep(Duration::from_secs(1)).await;
    signal.mark_ready();
    assert_eq!(gate.wait().await, Ok(()));
}
