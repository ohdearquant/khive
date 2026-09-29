//! The fetch path returns the identity representation or refuses.
//!
//! Some gzip decoders mishandle the optional header CRC (FHCRC): they feed the
//! two CRC bytes to the inflater, so a byte-capped read can return bytes that
//! are not a prefix of the plaintext with no error. Whether a given stream
//! errors or silently corrupts depends on header fields such as the mtime, so
//! these tests scan those fields instead of checking one fixture.

use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use khive_runtime::RuntimeError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::egress;
use crate::fetch::{run_one_hop, HopOutcome};

#[derive(Clone, Copy, PartialEq)]
enum HeaderCrc {
    Absent,
    Correct,
    Wrong,
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn gzip_body(plaintext: &[u8], header_crc: HeaderCrc, mtime: u32) -> Vec<u8> {
    let mut deflater =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    deflater.write_all(plaintext).expect("write deflate body");
    let compressed = deflater.finish().expect("finish deflate body");

    let flags = if header_crc == HeaderCrc::Absent {
        0_u8
    } else {
        0x02_u8
    };
    let mut header = vec![0x1f, 0x8b, 8, flags];
    header.extend_from_slice(&mtime.to_le_bytes());
    header.extend_from_slice(&[0, 255]);
    let mut gzip = header.clone();
    if header_crc != HeaderCrc::Absent {
        let crc16 = crc32(&header) as u16;
        let crc16 = if header_crc == HeaderCrc::Wrong {
            crc16 ^ 1
        } else {
            crc16
        };
        gzip.extend_from_slice(&crc16.to_le_bytes());
    }
    gzip.extend_from_slice(&compressed);
    gzip.extend_from_slice(&crc32(plaintext).to_le_bytes());
    gzip.extend_from_slice(&(plaintext.len() as u32).to_le_bytes());
    gzip
}

fn http_response(content_encoding: Option<&str>, body: &[u8]) -> Vec<u8> {
    let encoding = content_encoding
        .map(|value| format!("Content-Encoding: {value}\r\n"))
        .unwrap_or_default();
    let mut response = format!(
        "HTTP/1.1 200 OK\r\n{encoding}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

/// Serve one canned response and hand back the request head the client sent.
async fn spawn_once(response: Vec<u8>) -> (u16, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local address").port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept request");
        let mut request = Vec::with_capacity(4096);
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).await.expect("read request");
            assert!(read > 0, "client must send complete request headers");
            request.extend_from_slice(&chunk[..read]);
            assert!(request.len() <= 16 * 1024, "request headers too large");
        }
        assert!(request.starts_with(b"GET "), "expected a GET request");
        // The client may refuse and hang up before the write finishes.
        let _ = stream.write_all(&response).await;
        let _ = stream.shutdown().await;
        String::from_utf8(request).expect("ASCII request headers")
    });
    (port, server)
}

/// One capped GET through the production client builder. Returns the hop
/// result and the request head the server saw.
async fn capped_get(response: Vec<u8>, cap: usize) -> (Result<HopOutcome, RuntimeError>, String) {
    let (port, server) = spawn_once(response).await;
    let host = "identity.example";
    let client = egress::pinned_client(host, IpAddr::V4(Ipv4Addr::LOCALHOST), port)
        .expect("pinned client builds");
    let url = url::Url::parse(&format!("http://{host}:{port}/body")).expect("pinned test URL");
    let result = run_one_hop(
        &client,
        &url,
        reqwest::Method::GET,
        &[],
        cap as u64,
        Instant::now() + Duration::from_secs(5),
    )
    .await;
    let request = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server must receive the request")
        .expect("server task");
    (result, request)
}

fn first_eight_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

fn assert_content_encoding_refusal(label: &str, result: &Result<HopOutcome, RuntimeError>) {
    match result {
        Ok(outcome) => {
            let (bytes, truncated) = outcome.body.clone().unwrap_or_default();
            panic!(
                "{label}: expected a refusal, got status={} truncated={truncated} bytes={} first8_hex={}",
                outcome.status,
                bytes.len(),
                first_eight_hex(&bytes)
            );
        }
        Err(RuntimeError::InvalidInput(message))
            if message.starts_with("unsupported_content_encoding:") => {}
        Err(error) => panic!("{label}: expected unsupported_content_encoding, got {error}"),
    }
}

fn assert_offers_only_identity(label: &str, request: &str) {
    let offered: Vec<&str> = request
        .lines()
        .filter(|line| line.to_ascii_lowercase().starts_with("accept-encoding:"))
        .map(|line| line.split_once(':').expect("header line").1.trim())
        .collect();
    assert_eq!(
        offered,
        ["identity"],
        "{label}: the request must offer no compression: {request}"
    );
}

fn plaintext() -> Vec<u8> {
    let plaintext = b"Test FHCRC boundary; decode before truncating.\n".repeat(100);
    assert!(plaintext.len() > 1300);
    plaintext
}

fn caps(plaintext: &[u8]) -> [usize; 4] {
    let caps = [1, 1300, plaintext.len(), plaintext.len() + 1];
    assert!(
        caps.contains(&plaintext.len()),
        "the exact byte cap must be probed"
    );
    caps
}

/// Header mtime decides the header CRC, so it decides whether a mishandled
/// stream errors or corrupts. Every low byte value, plus values that set each
/// higher byte.
fn mtimes() -> Vec<u32> {
    (0_u32..256)
        .chain([
            0x100,
            0x1234,
            0x0001_0000,
            0x0100_0000,
            0xdead_beef,
            u32::MAX,
        ])
        .collect()
}

#[tokio::test]
async fn gzip_streams_with_a_header_crc_are_refused_across_mtimes_and_caps() {
    let plaintext = plaintext();
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);

    // Predeclared controls: each assertion must fail on the input it exists
    // to reject before it is trusted on the population.
    assert!(
        std::panic::catch_unwind(|| assert_content_encoding_refusal(
            "wrong-cause control",
            &Err(RuntimeError::InvalidInput(
                "transport_error: error decoding response body".into()
            ))
        ))
        .is_err(),
        "a decode error is not the named refusal"
    );
    assert!(
        std::panic::catch_unwind(|| assert_offers_only_identity(
            "offer control",
            "GET / HTTP/1.1\r\naccept-encoding: gzip\r\n\r\n"
        ))
        .is_err(),
        "an offer of gzip must fail the identity assertion"
    );

    let mut population = 0_usize;
    for header_crc in [HeaderCrc::Correct, HeaderCrc::Wrong] {
        for mtime in mtimes() {
            let gzip = gzip_body(&plaintext, header_crc, mtime);
            assert_eq!(gzip[3], 0x02, "FHCRC flag set");
            assert_eq!(&gzip[4..8], &mtime.to_le_bytes());
            for cap in caps(&plaintext) {
                let (result, request) = capped_get(http_response(Some("gzip"), &gzip), cap).await;
                let label = format!("fhcrc mtime={mtime:#x} cap={cap}");
                assert_content_encoding_refusal(&label, &result);
                assert_offers_only_identity(&label, &request);
                population += 1;
            }
        }
    }
    assert_eq!(population, 2 * mtimes().len() * 4);
}

#[tokio::test]
async fn plain_gzip_without_a_header_crc_is_refused() {
    let plaintext = plaintext();
    let mut population = 0_usize;
    for mtime in mtimes() {
        let gzip = gzip_body(&plaintext, HeaderCrc::Absent, mtime);
        assert_eq!(gzip[3], 0, "no header flags");
        for cap in caps(&plaintext) {
            let (result, request) = capped_get(http_response(Some("gzip"), &gzip), cap).await;
            let label = format!("plain gzip mtime={mtime:#x} cap={cap}");
            assert_content_encoding_refusal(&label, &result);
            assert_offers_only_identity(&label, &request);
            population += 1;
        }
    }
    assert_eq!(population, mtimes().len() * 4);
}

#[tokio::test]
async fn every_declared_content_coding_is_refused() {
    let body = gzip_body(&plaintext(), HeaderCrc::Correct, 7);
    for coding in [
        "gzip",
        "GZIP",
        " gzip ",
        "x-gzip",
        "br",
        "deflate",
        "zstd",
        "compress",
        "aes128gcm",
        "identity, gzip",
        "gzip, identity",
        "identity,identity,br",
    ] {
        let (result, _) = capped_get(http_response(Some(coding), &body), 64).await;
        assert_content_encoding_refusal(&format!("content-encoding {coding:?}"), &result);
    }
}

#[tokio::test]
async fn identity_responses_return_the_exact_plaintext_prefix_at_each_cap() {
    let plaintext = plaintext();
    // Undecoded gzip bytes served as the identity representation are just
    // bytes: they come back verbatim, never decoded.
    let gzip_as_identity = gzip_body(&plaintext, HeaderCrc::Correct, 2);
    for (label, body) in [
        ("plaintext", plaintext.clone()),
        ("gzip bytes as identity", gzip_as_identity),
    ] {
        for declared in [None, Some("identity"), Some("Identity"), Some("")] {
            for cap in [1, 50, 1300, body.len(), body.len() + 1] {
                let (result, request) = capped_get(http_response(declared, &body), cap).await;
                let label = format!("{label} content-encoding={declared:?} cap={cap}");
                let outcome = result.unwrap_or_else(|error| panic!("{label}: {error}"));
                assert_eq!(outcome.status, 200, "{label}");
                let (received, truncated) = outcome.body.expect("GET must return a body slot");
                let expected = cap.min(body.len());
                assert_eq!(received, body[..expected], "{label}");
                assert_eq!(truncated, cap < body.len(), "{label}");
                assert_offers_only_identity(&label, &request);
            }
        }
    }
}
