//! Production HTTP mechanics and settlement, below the loopback-refusing
//! egress boundary. These are source regressions, not egress replacements.
use super::*;
use crate::fetch::{effective_request_headers, run_one_hop, settle_with_request_headers};
use khive_runtime::{Namespace, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::Entity;
use reqwest::header::HeaderMap;
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const BODY: &[u8] = b"same representation bytes";
const MODIFIED: &str = "Mon, 21 Sep 2026 12:00:00 GMT";

fn fixture() -> (KhiveRuntime, NamespaceToken, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some("web-metadata-test".into()),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(WebPack::new(runtime.clone()));
    runtime.install_edge_rules(builder.build().unwrap().all_edge_rules());
    runtime
        .install_blob_store(Arc::new(
            khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0).unwrap(),
        ))
        .unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    (runtime, token, dir)
}

fn response_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "text/plain".parse().unwrap());
    headers.insert("etag", "old-etag".parse().unwrap());
    headers.insert("last-modified", MODIFIED.parse().unwrap());
    headers
}

async fn entity(runtime: &KhiveRuntime, token: &NamespaceToken, id: Uuid) -> Entity {
    runtime
        .entities(token)
        .unwrap()
        .get_entity(id)
        .await
        .unwrap()
        .unwrap()
}

async fn receipt(runtime: &KhiveRuntime, token: &NamespaceToken, reply: &Value) -> Value {
    runtime
        .notes(token)
        .unwrap()
        .get_note(Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap())
        .await
        .unwrap()
        .unwrap()
        .properties
        .unwrap()["request"]
        .clone()
}

async fn seed(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    url: &Url,
    headers: &[(String, String)],
) -> Uuid {
    let reply = settle_with_request_headers(
        runtime,
        token,
        "GET",
        url,
        200,
        &response_headers(),
        Some((BODY.to_vec(), false)),
        &[],
        true,
        headers,
    )
    .await
    .unwrap();
    Uuid::parse_str(reply["id"].as_str().unwrap()).unwrap()
}

async fn refresh(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    final_url: &Url,
    status: u16,
    headers: HeaderMap,
    hops: &[crate::fetch::RedirectHop],
) -> Value {
    let before = entity(runtime, token, id).await.properties.unwrap();
    let sent = refresh_request_headers(&before).unwrap();
    settle_refresh_with_request_headers(
        runtime,
        token,
        id,
        before["url"].as_str().unwrap(),
        before["blob_ref"].as_str().unwrap(),
        HopOutcome {
            status,
            final_url: final_url.clone(),
            headers,
            redirect_to: None,
            body: Some((if status == 304 { vec![] } else { BODY.to_vec() }, false)),
        },
        hops,
        &sent,
    )
    .await
    .unwrap()
}

// Control: guard metadata application by `changed`; each metadata-only arm fails.
#[tokio::test]
async fn same_body_refresh_updates_representation_metadata() {
    for field in [
        "content-type",
        "etag",
        "last-modified",
        "status",
        "absent-validators",
    ] {
        let (runtime, token, _dir) = fixture();
        let url = Url::parse("https://metadata.example/same").unwrap();
        let id = seed(&runtime, &token, &url, &[]).await;
        let before = entity(&runtime, &token, id).await;
        let roots = runtime
            .core()
            .attachments()
            .unwrap()
            .list_attachments(id)
            .await
            .unwrap();
        let mut headers = response_headers();
        let mut status = 200;
        match field {
            "content-type" => {
                headers.insert("content-type", "text/html".parse().unwrap());
            }
            "etag" => {
                headers.insert("etag", "new-etag".parse().unwrap());
            }
            "last-modified" => {
                headers.insert(
                    "last-modified",
                    "Tue, 22 Sep 2026 12:00:00 GMT".parse().unwrap(),
                );
            }
            "status" => {
                status = 203;
            }
            _ => {
                headers.remove("etag");
                headers.remove("last-modified");
            }
        }
        let reply = refresh(&runtime, &token, id, &url, status, headers.clone(), &[]).await;
        let after = entity(&runtime, &token, id).await;
        let properties = after.properties.as_ref().unwrap();
        for (header, property) in [
            ("content-type", "content_type"),
            ("etag", "etag"),
            ("last-modified", "last_modified"),
        ] {
            assert_eq!(
                properties[property],
                json!(headers.get(header).and_then(|value| value.to_str().ok())),
                "same-byte metadata must update {property}, arm={field}"
            );
        }
        assert_eq!(
            properties["status"], status,
            "same-byte response status must update"
        );
        assert_eq!(
            after.entity_type.as_deref(),
            Some(if field == "content-type" {
                "page"
            } else {
                "resource"
            }),
            "same-byte content type must reclassify the document"
        );
        assert_eq!(
            reply["changed"], false,
            "metadata does not change the body flag"
        );
        for key in ["blob_ref", "content_digest", "size", "fetched_at"] {
            assert_eq!(
                properties[key],
                before.properties.as_ref().unwrap()[key],
                "metadata-only refresh preserves {key}"
            );
        }
        assert_eq!(
            runtime
                .core()
                .attachments()
                .unwrap()
                .list_attachments(id)
                .await
                .unwrap(),
            roots,
            "metadata-only refresh must not rewrite body attachments"
        );
        assert_eq!(
            receipt(&runtime, &token, &reply).await["headers"],
            crate::fetch::extract_allowed_headers(&headers)
        );
    }
}

// Control: skip 304 metadata application; replacement validators remain stale.
#[tokio::test]
async fn not_modified_refresh_updates_supplied_headers() {
    for field in [
        "etag",
        "last-modified",
        "content-type",
        "unchanged",
        "absent",
    ] {
        let (runtime, token, _dir) = fixture();
        let url = Url::parse("https://metadata.example/validated").unwrap();
        let id = seed(&runtime, &token, &url, &[]).await;
        let before = entity(&runtime, &token, id).await;
        let roots = runtime
            .core()
            .attachments()
            .unwrap()
            .list_attachments(id)
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        match field {
            "etag" => {
                headers.insert("etag", "validated-etag".parse().unwrap());
            }
            "last-modified" => {
                headers.insert(
                    "last-modified",
                    "Tue, 22 Sep 2026 12:00:00 GMT".parse().unwrap(),
                );
            }
            "content-type" => {
                headers.insert("content-type", "text/html".parse().unwrap());
            }
            "unchanged" => {
                headers = response_headers();
            }
            _ => {}
        }
        let reply = refresh(&runtime, &token, id, &url, 304, headers.clone(), &[]).await;
        let after = entity(&runtime, &token, id).await;
        if matches!(field, "unchanged" | "absent") {
            assert_eq!(
                serde_json::to_value(&after).unwrap(),
                serde_json::to_value(&before).unwrap(),
                "304 with unchanged metadata must be receipt-only"
            );
        }
        let properties = after.properties.as_ref().unwrap();
        for (header, property) in [
            ("etag", "etag"),
            ("last-modified", "last_modified"),
            ("content-type", "content_type"),
        ] {
            let expected = headers
                .get(header)
                .map(|value| json!(value.to_str().unwrap()))
                .unwrap_or_else(|| before.properties.as_ref().unwrap()[property].clone());
            assert_eq!(
                properties[property], expected,
                "304 must apply supplied {property} and preserve omitted fields"
            );
        }
        assert_eq!(
            properties["status"], 200,
            "304 retains cached representation status"
        );
        assert_eq!(reply["changed"], false);
        for key in ["blob_ref", "content_digest", "size", "fetched_at"] {
            assert_eq!(
                properties[key],
                before.properties.as_ref().unwrap()[key],
                "304 metadata preserves {key}"
            );
        }
        assert_eq!(
            runtime
                .core()
                .attachments()
                .unwrap()
                .list_attachments(id)
                .await
                .unwrap(),
            roots,
            "304 metadata must leave body attachments untouched"
        );
        let request = receipt(&runtime, &token, &reply).await;
        assert_eq!(request["status"], 304);
        assert_eq!(
            request["headers"],
            crate::fetch::extract_allowed_headers(&headers)
        );
    }
}

#[tokio::test]
async fn redirected_refresh_updates_terminal_metadata_and_negotiation() {
    for redirect in [301, 302, 307, 308] {
        for response_status in [200, 304] {
            for existing_terminal in [false, true] {
                let (runtime, token, _dir) = fixture();
                let source_url = Url::parse("https://metadata.example/source").unwrap();
                let final_url = Url::parse("https://metadata.example/terminal").unwrap();
                let sent = vec![
                    ("Accept".into(), "application/json".into()),
                    ("Accept-Language".into(), "fr".into()),
                ];
                let source = seed(&runtime, &token, &source_url, &sent).await;
                let before = entity(&runtime, &token, source).await;
                if existing_terminal {
                    seed(&runtime, &token, &final_url, &[]).await;
                }
                let mut headers = HeaderMap::new();
                headers.insert("etag", "terminal-new-etag".parse().unwrap());
                headers.insert("content-type", "text/html".parse().unwrap());
                let reply = refresh(
                    &runtime,
                    &token,
                    source,
                    &final_url,
                    response_status,
                    headers,
                    &[crate::fetch::RedirectHop {
                        from: source_url.clone(),
                        to: final_url.clone(),
                        status: redirect,
                    }],
                )
                .await;
                let final_id = Uuid::parse_str(reply["final_id"].as_str().unwrap()).unwrap();
                assert_ne!(final_id, source);
                let final_entity = entity(&runtime, &token, final_id).await;
                let properties = final_entity.properties.unwrap();
                assert_eq!(
                    properties["etag"], "terminal-new-etag",
                    "redirect metadata belongs to the terminal document"
                );
                assert_eq!(
                    properties["request_headers"],
                    json!({"accept": ["application/json"], "accept-language": ["fr"]}),
                    "terminal refresh must retain the sent negotiation"
                );
                assert_eq!(properties["status"], 200);
                assert_eq!(final_entity.entity_type.as_deref(), Some("page"));
                assert_eq!(reply["changed"], !existing_terminal);
                let after = entity(&runtime, &token, source).await.properties.unwrap();
                for key in ["url", "etag", "content_type", "blob_ref", "request_headers"] {
                    assert_eq!(
                        after[key],
                        before.properties.as_ref().unwrap()[key],
                        "redirect must preserve source {key}"
                    );
                }
            }
        }
    }
}

async fn capture_requests() -> (
    Url,
    tokio::task::JoinHandle<Vec<BTreeMap<String, Vec<String>>>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "http://127.0.0.1:{}/variant",
        listener.local_addr().unwrap().port()
    ))
    .unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, etag) in [
            (200, Some("old-etag")),
            (200, Some("new-etag")),
            (304, Some("validated-etag")),
            (304, None),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut buffer = [0; 1024];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(
                    count > 0 && bytes.len() < 16_384,
                    "request headers must complete within the bound"
                );
                bytes.extend_from_slice(&buffer[..count]);
            }
            let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for line in String::from_utf8(bytes).unwrap().lines().skip(1) {
                if let Some((name, value)) = line.split_once(':') {
                    headers
                        .entry(name.to_ascii_lowercase())
                        .or_default()
                        .push(value.trim().to_string());
                }
            }
            requests.push(headers);
            let body = if status == 200 { BODY } else { &[] };
            let mut response = format!("HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n", body.len());
            if let Some(etag) = etag {
                response.push_str(&format!("ETag: {etag}\r\n"));
            }
            response.push_str("\r\n");
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
            socket.shutdown().await.unwrap();
        }
        requests
    });
    (url, task)
}

// Controls: drop stored negotiation replay; or reinstate the digest-only
// metadata guard, so the next actual request continues to send old-etag.
#[tokio::test]
async fn refresh_reuses_negotiation_and_updated_validator_on_next_http_request() {
    let (runtime, token, _dir) = fixture();
    let (url, captured) = capture_requests().await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let headers = effective_request_headers(
        &BTreeMap::from([("aCcEpT-LaNgUaGe".into(), "fr-CA,fr;q=0.8".into())]),
        Some("application/json"),
    )
    .unwrap();
    let outcome = run_one_hop(
        &client,
        &url,
        reqwest::Method::GET,
        &headers,
        4096,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    let fetched = settle_with_request_headers(
        &runtime,
        &token,
        "GET",
        &outcome.final_url,
        outcome.status,
        &outcome.headers,
        outcome.body,
        &[],
        true,
        &headers,
    )
    .await
    .unwrap();
    let id = Uuid::parse_str(fetched["id"].as_str().unwrap()).unwrap();
    let expected = json!({"accept": ["application/json"], "accept-language": ["fr-CA,fr;q=0.8"]});
    assert_eq!(
        receipt(&runtime, &token, &fetched).await["request_headers"],
        expected,
        "fetch receipt records negotiated request headers"
    );
    let mut refresh_receipts = Vec::new();
    for _ in 0..3 {
        let properties = entity(&runtime, &token, id).await.properties.unwrap();
        let sent = refresh_request_headers(&properties).unwrap();
        let outcome = run_one_hop(
            &client,
            &url,
            reqwest::Method::GET,
            &sent,
            4096,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
        let reply = settle_refresh_with_request_headers(
            &runtime,
            &token,
            id,
            url.as_str(),
            properties["blob_ref"].as_str().unwrap(),
            outcome,
            &[],
            &sent,
        )
        .await
        .unwrap();
        assert_eq!(reply["changed"], false);
        refresh_receipts.push(receipt(&runtime, &token, &reply).await);
    }
    let requests = tokio::time::timeout(Duration::from_secs(5), captured)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 4);
    for headers in &requests {
        assert_eq!(
            headers.get("accept"),
            Some(&vec!["application/json".to_string()]),
            "refresh must resend fetched Accept"
        );
        assert_eq!(
            headers.get("accept-language"),
            Some(&vec!["fr-CA,fr;q=0.8".to_string()]),
            "refresh must resend fetched Accept-Language"
        );
    }
    for (request, expected) in requests.iter().zip([
        None,
        Some("old-etag"),
        Some("new-etag"),
        Some("validated-etag"),
    ]) {
        assert_eq!(
            request
                .get("if-none-match")
                .and_then(|values| values.first())
                .map(String::as_str),
            expected,
            "next conditional request must use the latest response ETag"
        );
    }
    for request in refresh_receipts {
        assert_eq!(
            request["request_headers"], expected,
            "refresh receipt records the request actually sent"
        );
    }
}

#[tokio::test]
async fn fetch_and_refresh_receipts_allowlist_metadata() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://metadata.example/receipt").unwrap();
    let mut response = response_headers();
    response.insert("content-length", BODY.len().to_string().parse().unwrap());
    response.insert("set-cookie", "secret-cookie".parse().unwrap());
    response.insert("x-private", "secret-response".parse().unwrap());
    let sent = vec![
        ("Accept".into(), "application/json".into()),
        ("accept".into(), "text/plain;q=0.5".into()),
        ("Accept-Language".into(), "en".into()),
        ("Authorization".into(), "Bearer secret-request".into()),
        ("User-Agent".into(), "private-agent".into()),
    ];
    let fetched = settle_with_request_headers(
        &runtime,
        &token,
        "GET",
        &url,
        200,
        &response,
        Some((BODY.to_vec(), false)),
        &[],
        true,
        &sent,
    )
    .await
    .unwrap();
    let id = Uuid::parse_str(fetched["id"].as_str().unwrap()).unwrap();
    let properties = entity(&runtime, &token, id).await.properties.unwrap();
    let stored = crate::fetch::stored_negotiation_headers(&properties).unwrap();
    assert_eq!(
        stored.len(),
        3,
        "only negotiation header values are durable"
    );
    let expected =
        json!({"accept": ["application/json", "text/plain;q=0.5"], "accept-language": ["en"]});
    let refreshed = settle_refresh_with_request_headers(
        &runtime,
        &token,
        id,
        url.as_str(),
        properties["blob_ref"].as_str().unwrap(),
        HopOutcome {
            status: 304,
            final_url: url.clone(),
            headers: response.clone(),
            redirect_to: None,
            body: None,
        },
        &[],
        &sent,
    )
    .await
    .unwrap();
    for reply in [&fetched, &refreshed] {
        let request = receipt(&runtime, &token, reply).await;
        assert_eq!(
            request["request_headers"], expected,
            "receipt request metadata is negotiation-only"
        );
        assert_eq!(
            request["headers"],
            json!({"content-type": "text/plain", "content-length": BODY.len().to_string(), "etag": "old-etag", "last-modified": MODIFIED}),
            "receipt response metadata must use the allowlist"
        );
        let serialized = request.to_string();
        assert!(
            !serialized.contains("secret-") && !serialized.contains("private-agent"),
            "receipts must exclude credentials and unapproved headers"
        );
    }
}

#[tokio::test]
async fn head_preserves_cached_get_negotiation_and_unnegotiated_get_clears_it() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://metadata.example/head").unwrap();
    let original = vec![("Accept".into(), "application/json".into())];
    let id = seed(&runtime, &token, &url, &original).await;
    let head_headers = vec![("Accept".into(), "text/html".into())];
    let reply = settle_with_request_headers(
        &runtime,
        &token,
        "HEAD",
        &url,
        200,
        &response_headers(),
        None,
        &[],
        true,
        &head_headers,
    )
    .await
    .unwrap();
    let properties = entity(&runtime, &token, id).await.properties.unwrap();
    assert_eq!(
        crate::fetch::negotiation_headers(&refresh_request_headers(&properties).unwrap()),
        crate::fetch::negotiation_headers(&original),
        "HEAD must not replace cached GET negotiation"
    );
    assert_eq!(
        receipt(&runtime, &token, &reply).await["request_headers"],
        json!({"accept": ["text/html"]})
    );
    seed(&runtime, &token, &url, &[]).await;
    let properties = entity(&runtime, &token, id).await.properties.unwrap();
    assert!(
        crate::fetch::stored_negotiation_headers(&properties)
            .unwrap()
            .is_empty(),
        "a later unnegotiated GET must clear stale negotiation"
    );
}

#[tokio::test]
async fn overlapping_refresh_keeps_validators_with_the_settled_body() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://metadata.example/overlapping").unwrap();
    let id = seed(&runtime, &token, &url, &[]).await;
    let initial = entity(&runtime, &token, id).await;
    let initial_properties = initial.properties.unwrap();
    let initial_ref = initial_properties["blob_ref"].as_str().unwrap().to_owned();
    let first_request_headers = refresh_request_headers(&initial_properties).unwrap();

    let mut first_headers = response_headers();
    first_headers.insert("etag", "response-a-etag".parse().unwrap());
    first_headers.insert(
        "last-modified",
        "Tue, 22 Sep 2026 12:00:00 GMT".parse().unwrap(),
    );
    let first_outcome = HopOutcome {
        status: 200,
        final_url: url.clone(),
        headers: first_headers,
        redirect_to: None,
        body: Some((b"response A body".to_vec(), false)),
    };

    let (body_settled_tx, body_settled_rx) = tokio::sync::oneshot::channel();
    let (continue_tx, continue_rx) = tokio::sync::oneshot::channel();
    let first_runtime = runtime.clone();
    let first_token = token.clone();
    let first_url = url.clone();
    let first = tokio::spawn(async move {
        settle_refresh_with_request_headers_after_body_settlement(
            &first_runtime,
            &first_token,
            id,
            first_url.as_str(),
            &initial_ref,
            first_outcome,
            &[],
            &first_request_headers,
            async move {
                body_settled_tx
                    .send(())
                    .expect("the test is waiting for the first body settlement");
                continue_rx
                    .await
                    .expect("the test releases the first refresh after the second completes");
            },
        )
        .await
    });

    body_settled_rx
        .await
        .expect("the first refresh settles its body before pausing");
    let after_first_body = entity(&runtime, &token, id).await;
    let second_request_headers =
        refresh_request_headers(after_first_body.properties.as_ref().unwrap()).unwrap();
    let mut second_headers = response_headers();
    second_headers.insert("etag", "response-b-etag".parse().unwrap());
    second_headers.insert(
        "last-modified",
        "Wed, 23 Sep 2026 12:00:00 GMT".parse().unwrap(),
    );
    let second_reply = settle_refresh_with_request_headers(
        &runtime,
        &token,
        id,
        url.as_str(),
        after_first_body.properties.as_ref().unwrap()["blob_ref"]
            .as_str()
            .unwrap(),
        HopOutcome {
            status: 200,
            final_url: url.clone(),
            headers: second_headers,
            redirect_to: None,
            body: Some((b"response B body".to_vec(), false)),
        },
        &[],
        &second_request_headers,
    )
    .await
    .unwrap();
    let after_second = entity(&runtime, &token, id).await;

    continue_tx
        .send(())
        .expect("the first refresh is waiting after body settlement");
    let first_reply = first.await.unwrap().unwrap();
    let final_entity = entity(&runtime, &token, id).await;
    let final_properties = final_entity.properties.as_ref().unwrap();
    let second_properties = after_second.properties.as_ref().unwrap();

    assert_eq!(
        final_properties["blob_ref"], second_properties["blob_ref"],
        "the later completed response must retain its stored body"
    );
    assert!(
        final_properties["etag"] == second_properties["etag"],
        "validators must describe the response that supplied the stored body"
    );
    assert!(
        final_properties["last_modified"] == second_properties["last_modified"],
        "validators must describe the response that supplied the stored body"
    );
    let body_ref =
        khive_storage::ContentRef::from_hex(final_properties["blob_ref"].as_str().unwrap())
            .unwrap();
    let stored_body = crate::blob_store(&runtime)
        .unwrap()
        .get_bounded_verified(&body_ref, 64)
        .await
        .unwrap();
    assert_eq!(stored_body, b"response B body");
    assert_eq!(final_properties["etag"], "response-b-etag");
    assert_eq!(
        final_properties["last_modified"],
        "Wed, 23 Sep 2026 12:00:00 GMT"
    );
    assert_eq!(first_reply["lost_race"], true);
    assert_eq!(second_reply["lost_race"], false);
}
