use super::*;

const CREDENTIAL: &str = "mirror-read-credential";

fn key(byte: char) -> String {
    byte.to_string().repeat(64)
}

/// A one-connection raw HTTP server: answers `status_line` with `body` and returns its base URL
/// plus a receiver for the request text it read.
fn serve_once(
    status_line: &'static str,
    extra_headers: &'static str,
    body: String,
) -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            use std::io::{Read, Write};
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&request).into_owned());
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{addr}/admin/email-suppression"), rx)
}

#[tokio::test]
async fn reads_the_complete_set_with_the_bearer_credential() {
    let body = format!(
        r#"{{"as_of":"2026-10-17T08:00:00.123456+00:00","count":2,"recipient_keys":["{}","{}"]}}"#,
        key('a'),
        key('0')
    );
    let (url, request) = serve_once("200 OK", "", body);
    let set = fetch_remote_suppression_set(&url, CREDENTIAL)
        .await
        .unwrap();
    assert_eq!(set.as_of, "2026-10-17T08:00:00.123456+00:00");
    assert_eq!(set.recipient_keys, vec![key('a'), key('0')]);

    let request = request.recv().unwrap().to_ascii_lowercase();
    assert!(
        request.starts_with("get /admin/email-suppression "),
        "{request}"
    );
    assert!(
        request.contains(&format!("authorization: bearer {CREDENTIAL}")),
        "{request}"
    );
}

#[tokio::test]
async fn an_empty_set_is_a_successful_read() {
    let (url, _request) = serve_once(
        "200 OK",
        "",
        r#"{"as_of":"2026-10-17T08:00:00Z","count":0,"recipient_keys":[]}"#.to_string(),
    );
    let set = fetch_remote_suppression_set(&url, CREDENTIAL)
        .await
        .unwrap();
    assert!(set.recipient_keys.is_empty());
}

#[tokio::test]
async fn any_status_other_than_200_fails_the_read() {
    for (status_line, extra_headers, code) in [
        ("401 Unauthorized", "", "401"),
        ("403 Forbidden", "", "403"),
        ("500 Internal Server Error", "", "500"),
        ("503 Service Unavailable", "Retry-After: 5\r\n", "503"),
        (
            "302 Found",
            "Location: http://127.0.0.1:9/elsewhere\r\n",
            "302",
        ),
    ] {
        let (url, _request) = serve_once(
            status_line,
            extra_headers,
            r#"{"as_of":"2026-10-17T08:00:00Z","recipient_keys":[]}"#.to_string(),
        );
        let error = fetch_remote_suppression_set(&url, CREDENTIAL)
            .await
            .unwrap_err();
        assert!(error.contains(&format!("HTTP {code}")), "{error}");
        assert!(!error.contains(CREDENTIAL), "credential leaked: {error}");
    }
}

#[tokio::test]
async fn an_unreachable_mirror_fails_the_read() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let error = fetch_remote_suppression_set(&format!("http://{addr}/x"), CREDENTIAL)
        .await
        .unwrap_err();
    assert!(
        error.starts_with("suppression mirror request failed"),
        "{error}"
    );
    assert!(!error.contains(CREDENTIAL), "credential leaked: {error}");
}

#[tokio::test]
async fn a_declared_length_over_the_limit_fails_before_the_body_is_read() {
    let (url, _request) = serve_once(
        "200 OK",
        "",
        format!(
            r#"{{"as_of":"2026-10-17T08:00:00Z","recipient_keys":[],"pad":"{}"}}"#,
            " ".repeat(16)
        ),
    );
    // Control: the same server shape under the limit reads.
    assert!(fetch_remote_suppression_set(&url, CREDENTIAL).await.is_ok());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{{",
                MAX_RESPONSE_BYTES + 1
            );
            let _ = stream.write_all(head.as_bytes());
        }
    });
    let error = fetch_remote_suppression_set(&format!("http://{addr}/x"), CREDENTIAL)
        .await
        .unwrap_err();
    assert!(error.contains("-byte limit"), "{error}");
}

#[test]
fn a_well_formed_body_parses_and_unknown_fields_are_ignored() {
    let body = format!(
        r#"{{"as_of":"2026-10-17T08:00:00Z","recipient_keys":["{}"],"next":"later"}}"#,
        key('f')
    );
    let set = parse_remote_suppression_set(body.as_bytes()).unwrap();
    assert_eq!(set.recipient_keys, vec![key('f')]);
}

#[test]
fn a_malformed_body_fails_the_whole_read() {
    let good = key('b');
    let cases = [
        ("not json".to_string(), "not the expected JSON"),
        (
            format!(r#"{{"recipient_keys":["{good}"]}}"#),
            "not the expected JSON",
        ),
        (
            r#"{"as_of":"2026-10-17T08:00:00Z"}"#.to_string(),
            "not the expected JSON",
        ),
        (
            format!(r#"{{"as_of":"yesterday","recipient_keys":["{good}"]}}"#),
            "not RFC 3339",
        ),
        (
            format!(r#"{{"as_of":"2026-10-17T08:00:00Z","count":2,"recipient_keys":["{good}"]}}"#),
            "counts 2 keys but carries 1",
        ),
        (
            format!(
                r#"{{"as_of":"2026-10-17T08:00:00Z","recipient_keys":["{good}","{}"]}}"#,
                key('B')
            ),
            "key at position 1",
        ),
        (
            format!(
                r#"{{"as_of":"2026-10-17T08:00:00Z","recipient_keys":["{}"]}}"#,
                &good[..63]
            ),
            "key at position 0",
        ),
        (
            format!(
                r#"{{"as_of":"2026-10-17T08:00:00Z","recipient_keys":["{good}","{}"]}}"#,
                key('g')
            ),
            "key at position 1",
        ),
    ];
    for (body, needle) in cases {
        let error = parse_remote_suppression_set(body.as_bytes()).unwrap_err();
        assert!(error.contains(needle), "{body}: {error}");
    }
}
