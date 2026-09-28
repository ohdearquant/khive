use std::io::Write as _;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::fetch::run_one_hop;

#[derive(Clone, Copy)]
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

fn gzip_body(plaintext: &[u8], header_crc: HeaderCrc) -> Vec<u8> {
    let mut deflater =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    deflater.write_all(plaintext).expect("write deflate body");
    let compressed = deflater.finish().expect("finish deflate body");

    let flags = if matches!(header_crc, HeaderCrc::Absent) {
        0_u8
    } else {
        0x02_u8
    };
    let header = [0x1f, 0x8b, 8, flags, 0, 0, 0, 0, 0, 255];
    let mut gzip = header.to_vec();
    if !matches!(header_crc, HeaderCrc::Absent) {
        let crc16 = crc32(&header) as u16;
        let crc16 = if matches!(header_crc, HeaderCrc::Wrong) {
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

fn http_response(gzip: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        gzip.len()
    )
    .into_bytes();
    response.extend_from_slice(gzip);
    response
}

async fn spawn_once(response: Vec<u8>) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local address").port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept request");
        let mut request = [0_u8; 4096];
        let read = stream.read(&mut request).await.expect("read request");
        assert!(read > 0, "client must reach the local test server");
        let _ = stream.write_all(&response).await;
        let _ = stream.shutdown().await;
    });
    (port, server)
}

fn first_eight_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

fn assert_plaintext_prefix(plaintext: &[u8], decoded: &[u8]) {
    assert!(
        plaintext.starts_with(decoded),
        "decoded bytes are not a plaintext prefix: first8_hex={}",
        first_eight_hex(decoded)
    );
}

#[tokio::test]
async fn fhcrc_gzip_caps_yield_only_plaintext_prefixes_or_explicit_refusals() {
    let plaintext = b"Test FHCRC boundary; decode before truncating.\n".repeat(100);
    assert!(plaintext.len() > 1300);
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);

    // Predeclared control: this deliberately wrong first byte must trip the
    // same prefix assertion used for every successful network arm.
    assert!(
        std::panic::catch_unwind(|| assert_plaintext_prefix(&plaintext, b"S")).is_err(),
        "the nonprefix control must fail the prefix assertion"
    );

    let plain = gzip_body(&plaintext, HeaderCrc::Absent);
    let correct = gzip_body(&plaintext, HeaderCrc::Correct);
    let wrong = gzip_body(&plaintext, HeaderCrc::Wrong);
    assert_eq!(plain[3], 0);
    assert_eq!(correct[3], 0x02);
    assert_eq!(wrong[3], 0x02);
    assert_eq!(&correct[..10], &wrong[..10]);
    assert_eq!(correct[10] ^ 1, wrong[10]);
    assert_eq!(correct[11], wrong[11]);

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .gzip(true)
        .build()
        .expect("gzip client");
    let caps = [1, 1300, plaintext.len() + 1];
    let mut observations = Vec::new();

    for (variant, gzip) in [
        ("plain", plain),
        ("fhcrc_correct", correct),
        ("fhcrc_wrong", wrong),
    ] {
        for cap in caps {
            let (port, server) = spawn_once(http_response(&gzip)).await;
            let url =
                url::Url::parse(&format!("http://127.0.0.1:{port}/gzip")).expect("local test URL");
            let result = run_one_hop(
                &client,
                &url,
                reqwest::Method::GET,
                &[],
                cap as u64,
                Instant::now() + Duration::from_secs(5),
            )
            .await;
            tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .expect("server must receive the request")
                .expect("server task");

            match &result {
                Ok(outcome) => match &outcome.body {
                    Some((decoded, truncated)) => println!(
                        "fhcrc_probe variant={variant} cap={cap} status={} truncated={truncated} bytes={} first8_hex={} error=none",
                        outcome.status,
                        decoded.len(),
                        first_eight_hex(decoded)
                    ),
                    None => println!(
                        "fhcrc_probe variant={variant} cap={cap} status={} truncated=none bytes=none first8_hex=none error=missing_GET_body",
                        outcome.status
                    ),
                },
                Err(error) => println!(
                    "fhcrc_probe variant={variant} cap={cap} status=none truncated=none bytes=none first8_hex=none error={error}"
                ),
            }
            observations.push((variant, cap, result));
        }
    }

    for (variant, cap, result) in observations {
        match result {
            Ok(outcome) => {
                assert_eq!(outcome.status, 200, "{variant} cap={cap}");
                let (decoded, truncated) = outcome.body.expect("GET must return a body slot");
                assert_plaintext_prefix(&plaintext, &decoded);
                assert_eq!(
                    decoded.len(),
                    cap.min(plaintext.len()),
                    "{variant} cap={cap}"
                );
                assert_eq!(truncated, cap < plaintext.len(), "{variant} cap={cap}");
            }
            Err(error) => {
                assert!(
                    !error.to_string().is_empty(),
                    "{variant} cap={cap}: refusal must carry an error"
                );
                assert_ne!(
                    variant, "plain",
                    "the ordinary gzip positive control refused at cap={cap}: {error}"
                );
            }
        }
    }
}
