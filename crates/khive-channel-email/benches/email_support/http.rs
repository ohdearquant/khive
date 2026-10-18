use std::time::{Duration, Instant};

use khive_channel_email::suppression_mirror::{fetch_remote_suppression_set, RemoteSuppressionSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

const AS_OF: &str = "2026-10-17T08:00:00Z";
const CREDENTIAL: &str = "benchmark-synthetic-only";
const SAMPLE_DEADLINE: Duration = Duration::from_secs(35);

pub fn executor() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .expect("HTTP benchmark executor")
}

pub fn require_loopback_proxy_bypass() {
    // reqwest 0.12.28 / hyper-util 0.1.20 prefer uppercase, including an empty value.
    // Conservative exact tokens avoid claiming support for the full proxy grammar.
    let bypass = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .ok()
        .is_some_and(|raw| {
            raw.split(',')
                .map(str::trim)
                .any(|part| part == "*" || part == "127.0.0.1")
        });
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        assert!(std::env::var_os(name).is_none() || bypass, "HTTP benchmark refuses {name} without an effective NO_PROXY/no_proxy exact 127.0.0.1 or * bypass; no proxy values are logged");
    }
    // Feature unification can enable native system-proxy lookup on these platforms.
    assert!(!cfg!(any(target_os = "macos", windows)) || bypass, "HTTP benchmark on macOS/Windows requires effective NO_PROXY/no_proxy exact 127.0.0.1 or * to bypass possible system proxies");
}

pub struct Fixture {
    response: Vec<u8>,
    expected: RemoteSuppressionSet,
}
pub struct Prepared {
    listener: TcpListener,
    url: String,
}
pub struct Outcome {
    pub elapsed: Duration,
    result: Result<RemoteSuppressionSet, String>,
    request: Vec<u8>,
    port: u16,
}

impl Fixture {
    pub fn new(count: usize) -> Self {
        assert!([1_000, 10_000].contains(&count));
        require_loopback_proxy_bypass();
        let expected = RemoteSuppressionSet {
            as_of: AS_OF.to_string(),
            recipient_keys: (0..count).map(|index| format!("{index:064x}")).collect(),
        };
        let body = serde_json::to_vec(&serde_json::json!({"as_of": AS_OF, "count": count, "recipient_keys": &expected.recipient_keys})).expect("generated response JSON");
        assert!(body.len() < 1024 * 1024);
        let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(&body);
        Self { response, expected }
    }
    pub fn prepare(&self, rt: &Runtime) -> Prepared {
        require_loopback_proxy_bypass();
        let listener = rt
            .block_on(TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)))
            .expect("bind private loopback stub");
        let address = listener.local_addr().expect("stub address");
        assert!(address.ip().is_loopback());
        Prepared {
            listener,
            url: format!("http://127.0.0.1:{}/suppression", address.port()),
        }
    }
    pub fn run(&self, rt: &Runtime, prepared: Prepared) -> Outcome {
        let port = prepared.listener.local_addr().expect("stub address").port();
        // Both futures belong to this scope: timeout/unwind drops the sockets, with no detached task.
        let joined = rt.block_on(async {
            tokio::time::timeout(SAMPLE_DEADLINE, async {
                tokio::join!(
                    async {
                        let start = Instant::now();
                        let result = fetch_remote_suppression_set(&prepared.url, CREDENTIAL).await;
                        (start.elapsed(), result)
                    },
                    self.serve(&prepared.listener)
                )
            })
            .await
        });
        let ((elapsed, result), request) = joined.expect("bounded local HTTP sample timed out");
        Outcome {
            elapsed,
            result,
            request: request.expect("bounded loopback stub"),
            port,
        }
    }
    async fn serve(&self, listener: &TcpListener) -> Result<Vec<u8>, String> {
        let (mut stream, peer) = listener.accept().await.map_err(|e| e.to_string())?;
        if !peer.ip().is_loopback() {
            return Err("non-loopback stub peer".to_string());
        }
        let mut request = Vec::with_capacity(8192);
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let mut chunk = [0_u8; 512];
            let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
            if n == 0 || request.len() + n > 8192 {
                return Err("incomplete or oversized stub request".to_string());
            }
            request.extend_from_slice(&chunk[..n]);
        }
        stream
            .write_all(&self.response)
            .await
            .map_err(|e| e.to_string())?;
        stream.shutdown().await.map_err(|e| e.to_string())?;
        Ok(request)
    }
    pub fn validate(&self, outcome: &Outcome) {
        assert_eq!(
            outcome
                .result
                .as_ref()
                .expect("suppression fetch succeeded"),
            &self.expected
        );
        let request = std::str::from_utf8(&outcome.request).expect("HTTP request headers");
        let mut lines = request.split("\r\n");
        assert_eq!(lines.next(), Some("GET /suppression HTTP/1.1"));
        let header = |name: &str| {
            let values: Vec<_> = request
                .split("\r\n")
                .filter_map(|line| line.split_once(':'))
                .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.trim())
                .collect();
            assert_eq!(values.len(), 1, "exactly one {name} header");
            values[0]
        };
        assert_eq!(header("authorization"), format!("Bearer {CREDENTIAL}"));
        assert_eq!(header("host"), format!("127.0.0.1:{}", outcome.port));
        assert_eq!(header("accept"), "application/json");
    }
}
