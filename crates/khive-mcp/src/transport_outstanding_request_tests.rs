use super::*;

const SMALL_POPULATION: usize = 64;
const LARGE_POPULATION: usize = 1024;

#[derive(Clone, Copy)]
struct TrackerWork {
    contains: usize,
    admit: usize,
    retire: usize,
    sweep: usize,
}

fn request_id(value: usize) -> RequestId {
    RequestId::Number(i64::try_from(value).expect("test request id must fit in i64"))
}

fn populated_tracker(population: usize) -> OutstandingRequests {
    let mut outstanding = OutstandingRequests::default();
    let admitted_at = Instant::now();
    for value in 0..population {
        assert!(outstanding.admit(request_id(value), admitted_at, population));
    }
    outstanding.entries.take_element_touches();
    outstanding
}

fn measure_tracker_work(population: usize) -> TrackerWork {
    let mut outstanding = populated_tracker(population);

    assert!(!outstanding.contains(&request_id(population + 1)));
    let contains = outstanding.entries.take_element_touches();

    assert!(outstanding.admit(request_id(population), Instant::now(), population + 1));
    let admit = outstanding.entries.take_element_touches();

    outstanding.retire(&request_id(population / 2));
    let retire = outstanding.entries.take_element_touches();

    outstanding.drop_stale(Some(std::time::Duration::from_secs(60)));
    let sweep = outstanding.entries.take_element_touches();

    TrackerWork {
        contains,
        admit,
        retire,
        sweep,
    }
}

// Exact counts are the oracle: a counter that stops incrementing reads 0
// and a scan reads the population, so neither can pass as keyed work.
fn assert_keyed_work(
    operation: &str,
    expected_touches: usize,
    small_touches: usize,
    large_touches: usize,
) {
    assert!(
        small_touches == expected_touches && large_touches == expected_touches,
        "{operation} must touch exactly {expected_touches} element(s) at any population: \
             {SMALL_POPULATION} entries touched {small_touches}, {LARGE_POPULATION} entries \
             touched {large_touches}"
    );
}

#[test]
fn outstanding_request_tracker_work_is_population_independent() {
    let small = measure_tracker_work(SMALL_POPULATION);
    let large = measure_tracker_work(LARGE_POPULATION);

    // contains: one lookup. admit: the duplicate check, the newest link
    // update, the insert. retire (middle): the removal plus both neighbour
    // links. sweep with nothing stale: one look at the oldest entry.
    for (operation, expected_touches, small_touches, large_touches) in [
        ("contains", 1, small.contains, large.contains),
        ("admit", 3, small.admit, large.admit),
        ("retire", 3, small.retire, large.retire),
        ("non-stale sweep", 1, small.sweep, large.sweep),
    ] {
        assert_keyed_work(operation, expected_touches, small_touches, large_touches);
    }
}

#[test]
fn outstanding_request_tracker_rejects_admission_past_capacity() {
    let mut outstanding = OutstandingRequests::default();
    let admitted_at = Instant::now();

    assert!(outstanding.admit(rmcp::model::RequestId::Number(1), admitted_at, 2,));
    assert!(outstanding.admit(rmcp::model::RequestId::Number(2), admitted_at, 2,));
    assert!(!outstanding.admit(rmcp::model::RequestId::Number(3), admitted_at, 2,));
    assert_eq!(outstanding.len(), 2);
}

#[test]
fn stale_bookkeeping_keeps_request_id_reserved_until_response() {
    let mut outstanding = OutstandingRequests::default();
    let old = RequestId::Number(1);
    let other = RequestId::Number(2);
    let ttl = std::time::Duration::from_secs(1);
    assert!(outstanding.admit(old.clone(), Instant::now() - ttl * 2, 2));
    outstanding.drop_stale(Some(ttl));
    assert_eq!(outstanding.len(), 0, "stale freshness row is pruned");
    assert!(outstanding.admit(other.clone(), Instant::now(), 2));
    outstanding.retire(&other);

    assert!(outstanding.contains(&old), "old handler still owns its id");
    assert!(
        !outstanding.admit(old.clone(), Instant::now(), 2),
        "another admission must not reuse an id held by a stale live handler"
    );
    outstanding.retire(&old);
    assert!(outstanding.admit(old, Instant::now(), 2));
}

#[tokio::test]
async fn malformed_stdio_reply_has_a_write_deadline_even_without_peer_reads() {
    use rmcp::transport::{async_rw::AsyncRwTransport, Transport as _};
    use tokio::io::AsyncWriteExt;

    let root = tokio_util::sync::CancellationToken::new();
    // The invalid-request reply is larger than this pipe. The client keeps
    // the read half open but never drains it. Valid JSON with an invalid
    // message shape reaches rmcp 2.2's reply path; syntax errors are ignored.
    let (server_io, mut client_io) = tokio::io::duplex(16);
    let (read, write) = tokio::io::split(server_io);
    let write = DeadlineWriter::new(write, std::time::Duration::from_millis(50), root.clone());
    let mut transport = AsyncRwTransport::new_server(read, write);
    client_io
        .write_all(b"{}\n")
        .await
        .expect("send an invalid request line");

    let received = tokio::time::timeout(std::time::Duration::from_secs(1), transport.receive())
        .await
        .expect("invalid-request write must not pin the receive loop");
    assert!(
        received.is_none(),
        "write timeout must close the inner transport"
    );
    assert!(root.is_cancelled(), "write timeout must cancel the session");
}

#[test]
fn initialize_does_not_advertise_protocol_without_discovery_support() {
    let mut info = rmcp::model::InitializeResult::new(rmcp::model::ServerCapabilities::default());
    info.protocol_version = rmcp::model::ProtocolVersion::V_2026_07_28;
    let mut message = rmcp::model::ServerJsonRpcMessage::response(
        rmcp::model::ServerResult::InitializeResult(info),
        request_id(1),
    );

    cap_initialize_protocol_version(&mut message);
    let rmcp::model::JsonRpcMessage::Response(response) = message else {
        panic!("expected initialize response");
    };
    let rmcp::model::ServerResult::InitializeResult(info) = response.result else {
        panic!("expected initialize result");
    };
    assert_eq!(
        info.protocol_version,
        rmcp::model::ProtocolVersion::V_2025_11_25,
    );
}

#[tokio::test]
async fn stdio_initialize_caps_newer_client_version_on_wire() {
    use rmcp::ServiceExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    struct ProbeServer;
    impl rmcp::ServerHandler for ProbeServer {}

    let root = tokio_util::sync::CancellationToken::new();
    let (server_io, client_io) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(server_io);
    let transport = CancelOnEofTransport::with_idle_timeout(
        rmcp::transport::async_rw::AsyncRwTransport::new_server(read, write),
        root.clone(),
        None,
        Some(std::time::Duration::from_secs(1)),
        None,
    );
    let (client_read, mut client_write) = tokio::io::split(client_io);
    client_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2026-07-28\",\"capabilities\":{},\"clientInfo\":{\"name\":\"probe\",\"version\":\"1\"}}}\n")
            .await
            .expect("send newer initialize request");
    let running = ProbeServer
        .serve_with_ct(transport, root.clone())
        .await
        .expect("initialize the stdio service");
    let mut client_read = tokio::io::BufReader::new(client_read);
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        client_read.read_line(&mut line),
    )
    .await
    .expect("initialize reply deadline")
    .expect("read initialize reply");
    let reply: serde_json::Value = serde_json::from_str(&line).expect("initialize JSON-RPC");
    assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
    root.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), running.waiting()).await;
}

fn poison_tracker(tracker: &Arc<Mutex<OutstandingRequests>>) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = tracker
            .lock()
            .expect("tracker mutex must initially be healthy");
        panic!("deliberately poison the tracker mutex");
    }));
    assert!(
        result.is_err(),
        "the poisoning fixture must panic while holding the mutex"
    );
}

#[tokio::test]
async fn poisoned_tracker_fails_closed_on_admission_and_retirement() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use rmcp::transport::Transport as _;
    use tokio::io::AsyncWriteExt;

    let root = tokio_util::sync::CancellationToken::new();
    let (server_io, mut client_io) = tokio::io::duplex(4096);
    let (server_read, server_write) = tokio::io::split(server_io);
    let mut transport = CancelOnEofTransport::with_idle_timeout(
        AsyncRwTransport::new_server(server_read, server_write),
        root.clone(),
        None,
        None,
        None,
    );
    let tracker = transport.in_flight_handle();
    poison_tracker(&tracker);

    client_io
            .write_all(
                br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"probe","arguments":{}}}
"#,
            )
            .await
            .expect("write the request used to exercise poisoned admission");

    assert!(
        transport.receive().await.is_none(),
        "poisoned admission must return transport EOF instead of handing the request to rmcp"
    );
    assert!(
        root.is_cancelled(),
        "poisoned admission must cancel the rmcp root token"
    );

    let retirement_root = tokio_util::sync::CancellationToken::new();
    let mut retirement = ObligationRetirementGuard {
        in_flight: tracker,
        id: Some(rmcp::model::RequestId::Number(1)),
        obligation_ttl: None,
        root: retirement_root.clone(),
    };
    retirement.retire();
    assert!(
        retirement_root.is_cancelled(),
        "poisoned retirement must cancel the session instead of consuming the id silently"
    );
}
