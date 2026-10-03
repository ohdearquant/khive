//! Bounded loopback HTTP fixtures with byte-exact request capture.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify};
use tokio::task::{JoinHandle, JoinSet};

const MAX_HEADER_BYTES: usize = 16_384;
const MAX_REQUEST_BODY_BYTES: usize = 131_072;
const FIXTURE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub(crate) struct CapturedRequest {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl CapturedRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
    /// Release with `notify_one`, which also preserves a release before the wait.
    pub(crate) gate: Option<Arc<Notify>>,
}

impl Reply {
    pub(crate) fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.as_bytes().to_vec(),
            gate: None,
        }
    }
}

struct Captures {
    requests: Mutex<Vec<CapturedRequest>>,
    changed: Notify,
    outcome: Mutex<Option<Result<(), String>>>,
}

pub(crate) struct ScriptedServer {
    url: String,
    captures: Arc<Captures>,
    worker: JoinHandle<()>,
}

impl ScriptedServer {
    pub(crate) async fn start(replies: Vec<Reply>) -> Result<Self, String> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| error.to_string())?;
        let address = listener.local_addr().map_err(|error| error.to_string())?;
        let captures = Arc::new(Captures {
            requests: Mutex::new(Vec::new()),
            changed: Notify::new(),
            outcome: Mutex::new(None),
        });
        let worker_captures = Arc::clone(&captures);
        let worker = tokio::spawn(async move {
            let outcome = serve(listener, replies, Arc::clone(&worker_captures)).await;
            *worker_captures.outcome.lock().await = Some(outcome);
            worker_captures.changed.notify_one();
        });
        Ok(Self {
            url: format!("http://{address}"),
            captures,
            worker,
        })
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub(crate) async fn requests(&self) -> Vec<CapturedRequest> {
        self.captures.requests.lock().await.clone()
    }

    pub(crate) async fn wait_for_requests(&self, count: usize) -> Result<(), String> {
        tokio::time::timeout(FIXTURE_TIMEOUT, async {
            loop {
                if self.captures.requests.lock().await.len() >= count {
                    return Ok(());
                }
                if let Some(outcome) = self.captures.outcome.lock().await.clone() {
                    return match outcome {
                        Err(error) => Err(error),
                        Ok(()) => Err(format!("server finished before capturing {count} requests")),
                    };
                }
                self.captures.changed.notified().await;
            }
        })
        .await
        .map_err(|_| format!("timed out waiting for {count} fixture requests"))?
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

async fn serve(
    listener: TcpListener,
    replies: Vec<Reply>,
    captures: Arc<Captures>,
) -> Result<(), String> {
    let mut workers = JoinSet::new();
    for reply in replies {
        let (mut stream, _) = tokio::time::timeout(FIXTURE_TIMEOUT, listener.accept())
            .await
            .map_err(|_| "timed out accepting a fixture request".to_owned())?
            .map_err(|error| error.to_string())?;
        let captures = Arc::clone(&captures);
        workers.spawn(async move {
            let request = tokio::time::timeout(FIXTURE_TIMEOUT, read_request(&mut stream))
                .await
                .map_err(|_| "timed out reading a fixture request".to_owned())??;
            captures.requests.lock().await.push(request);
            captures.changed.notify_one();
            if let Some(gate) = &reply.gate {
                tokio::time::timeout(FIXTURE_TIMEOUT, gate.notified())
                    .await
                    .map_err(|_| "timed out waiting for the fixture response gate".to_owned())?;
            }
            tokio::time::timeout(FIXTURE_TIMEOUT, write_reply(&mut stream, reply))
                .await
                .map_err(|_| "timed out writing a fixture response".to_owned())?
        });
    }
    while let Some(outcome) = workers.join_next().await {
        outcome.map_err(|error| error.to_string())??;
    }
    Ok(())
}

async fn read_request(stream: &mut TcpStream) -> Result<CapturedRequest, String> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let end = position + 4;
            if end > MAX_HEADER_BYTES {
                return Err("fixture request headers exceed their bound".into());
            }
            break end;
        }
        if bytes.len() >= MAX_HEADER_BYTES {
            return Err("fixture request headers exceed their bound".into());
        }
        let count = stream
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("fixture request ended before the headers".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    };
    let text = std::str::from_utf8(&bytes[..header_end]).map_err(|error| error.to_string())?;
    let mut lines = text.split("\r\n");
    let mut request_line = lines
        .next()
        .ok_or("fixture request has no request line")?
        .split_whitespace();
    let method = request_line.next().ok_or("missing method")?.to_owned();
    let target = request_line.next().ok_or("missing target")?.to_owned();
    if request_line.next() != Some("HTTP/1.1") || request_line.next().is_some() {
        return Err("fixture requires an HTTP/1.1 request line".into());
    }
    let mut headers = Vec::new();
    let mut content_length = None;
    for line in lines.take_while(|line| !line.is_empty()) {
        let (key, value) = line.split_once(':').ok_or("malformed fixture header")?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        if key == "transfer-encoding" {
            return Err("fixture captures fixed-length request bodies only".into());
        }
        if key == "content-length" {
            if content_length.is_some() {
                return Err("duplicate fixture Content-Length".into());
            }
            content_length = Some(value.parse::<usize>().map_err(|error| error.to_string())?);
        }
        headers.push((key, value));
    }
    let body_len = content_length.unwrap_or(0);
    if body_len > MAX_REQUEST_BODY_BYTES {
        return Err("fixture request body exceeds its bound".into());
    }
    let request_end = header_end + body_len;
    while bytes.len() < request_end {
        let remaining = request_end - bytes.len();
        let count = stream
            .read(&mut chunk[..remaining.min(4096)])
            .await
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("fixture request ended before its declared body".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(CapturedRequest {
        method,
        target,
        headers,
        body: bytes[header_end..request_end].to_vec(),
    })
}

async fn write_reply(stream: &mut TcpStream, reply: Reply) -> Result<(), String> {
    let mut head = format!(
        "HTTP/1.1 {} Fixture\r\nConnection: close\r\nContent-Length: {}\r\n",
        reply.status,
        reply.body.len()
    );
    for (key, value) in reply.headers {
        if key.contains('\r') || key.contains('\n') || value.contains('\r') || value.contains('\n')
        {
            return Err("fixture reply header contains a line break".into());
        }
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(&reply.body)
        .await
        .map_err(|error| error.to_string())?;
    stream.shutdown().await.map_err(|error| error.to_string())
}
