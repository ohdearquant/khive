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

#[tokio::test]
async fn first_fetch_then_refresh_keeps_negotiation_on_the_rooted_body() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://metadata.example/first-fetch").unwrap();
    let sent = vec![("Accept-Language".to_owned(), "fr-CA".to_owned())];
    let id = seed(&runtime, &token, &url, &sent).await;
    let fetched = entity(&runtime, &token, id).await;
    let properties = fetched.properties.as_ref().unwrap();
    assert_eq!(
        fetched.content_ref.as_deref(),
        properties["blob_ref"].as_str()
    );
    assert_eq!(
        properties["request_headers"],
        json!({"accept-language": ["fr-CA"], "accept-encoding": ["identity"]})
    );

    let mut headers = response_headers();
    headers.insert("etag", "refreshed-etag".parse().unwrap());
    let reply = refresh(&runtime, &token, id, &url, 304, headers, &[]).await;
    let refreshed = entity(&runtime, &token, id).await;
    let properties = refreshed.properties.as_ref().unwrap();
    assert_eq!(
        properties["blob_ref"],
        fetched.properties.unwrap()["blob_ref"]
    );
    assert_eq!(
        properties["request_headers"],
        json!({"accept-language": ["fr-CA"], "accept-encoding": ["identity"]})
    );
    assert_eq!(properties["etag"], "refreshed-etag");
    assert_eq!(reply["changed"], false);
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
                200,
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
                json!({"accept": ["application/json"], "accept-language": ["fr"], "accept-encoding": ["identity"]}),
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

// A target's 304 cannot validate the source URI's cached body. No redirect
// endpoint, terminal row, or receipt may be written before this refusal.
#[tokio::test]
async fn redirected_304_preserves_source_and_terminal_rows() {
    for redirect in [301, 302, 307, 308] {
        for existing_terminal in [false, true] {
            let (runtime, token, _dir) = fixture();
            let source_url = Url::parse("https://metadata.example/source").unwrap();
            let final_url = Url::parse("https://metadata.example/terminal").unwrap();
            let sent = vec![("Accept-Language".into(), "fr".into())];
            let source = seed(&runtime, &token, &source_url, &sent).await;
            let source_before = entity(&runtime, &token, source).await;
            let final_canonical = crate::identity::canonicalize(final_url.clone());
            let final_id = crate::identity::document_id(
                crate::identity::site_id(&khive_types::Namespace::local(), &final_canonical),
                &crate::identity::path_and_query(&final_canonical),
            );
            if existing_terminal {
                assert_eq!(seed(&runtime, &token, &final_url, &[]).await, final_id);
            }
            let terminal_before = runtime
                .entities(&token)
                .unwrap()
                .get_entity(final_id)
                .await
                .unwrap();
            let receipt_before = latest_receipt(&runtime, &token, source).await.unwrap();
            let mut headers = HeaderMap::new();
            headers.insert("etag", "terminal-new-etag".parse().unwrap());
            headers.insert("content-type", "text/html".parse().unwrap());
            let error = settle_refresh_with_request_headers(
                &runtime,
                &token,
                source,
                source_url.as_str(),
                source_before.properties.as_ref().unwrap()["blob_ref"]
                    .as_str()
                    .unwrap(),
                HopOutcome {
                    status: 304,
                    final_url: final_url.clone(),
                    headers,
                    redirect_to: None,
                    body: None,
                },
                &[crate::fetch::RedirectHop {
                    from: source_url.clone(),
                    to: final_url.clone(),
                    status: redirect,
                }],
                &refresh_request_headers(source_before.properties.as_ref().unwrap()).unwrap(),
            )
            .await
            .expect_err("a redirected 304 must be refused");
            assert!(error.to_string().contains("redirected_not_modified"));
            let source_after = entity(&runtime, &token, source).await;
            let terminal_after = runtime
                .entities(&token)
                .unwrap()
                .get_entity(final_id)
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_value(source_after).unwrap(),
                serde_json::to_value(source_before).unwrap()
            );
            assert_eq!(
                serde_json::to_value(terminal_after).unwrap(),
                serde_json::to_value(terminal_before).unwrap()
            );
            assert_eq!(
                latest_receipt(&runtime, &token, source).await.unwrap(),
                receipt_before
            );
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
            let mut response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n",
                body.len()
            );
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
    let expected = json!({"accept": ["application/json"], "accept-language": ["fr-CA,fr;q=0.8"], "accept-encoding": ["identity"]});
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
        assert_eq!(
            headers.get("accept-encoding"),
            Some(&vec!["identity".to_string()]),
            "the built client must send its fixed encoding on fetch and refresh"
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
        4,
        "only negotiation header values are durable"
    );
    let expected = json!({"accept": ["application/json", "text/plain;q=0.5"], "accept-language": ["en"], "accept-encoding": ["identity"]});
    let mut conditional_sent = sent.clone();
    conditional_sent.push(("If-None-Match".into(), "old-etag".into()));
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
        &conditional_sent,
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
        crate::fetch::recorded_negotiation_headers(&original),
        "HEAD must not replace cached GET negotiation"
    );
    assert_eq!(
        receipt(&runtime, &token, &reply).await["request_headers"],
        json!({"accept": ["text/html"], "accept-encoding": ["identity"]})
    );
    seed(&runtime, &token, &url, &[]).await;
    let properties = entity(&runtime, &token, id).await.properties.unwrap();
    assert_eq!(
        crate::fetch::stored_negotiation_headers(&properties).unwrap(),
        vec![("accept-encoding".to_string(), "identity".to_string())],
        "a later unnegotiated GET clears caller fields but retains the fixed encoding"
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

#[tokio::test]
async fn redirected_refresh_keeps_a_get_written_after_the_terminal_request() {
    let (runtime, token, _dir) = fixture();
    let source_url = Url::parse("https://metadata.example/redirect-source").unwrap();
    let terminal_url = Url::parse("https://metadata.example/redirect-target").unwrap();
    let source_id = seed(&runtime, &token, &source_url, &[]).await;
    let terminal_id = seed(&runtime, &token, &terminal_url, &[]).await;
    let source_before = entity(&runtime, &token, source_id).await;
    let source_properties = source_before.properties.as_ref().unwrap();
    let source_ref = source_properties["blob_ref"].as_str().unwrap().to_owned();
    let request_headers = refresh_request_headers(source_properties).unwrap();
    let redirect_hops = [crate::fetch::RedirectHop {
        from: source_url.clone(),
        to: terminal_url.clone(),
        status: 302,
    }];

    let mut older_refresh_headers = response_headers();
    older_refresh_headers.insert("etag", "older-refresh".parse().unwrap());
    older_refresh_headers.insert("vary", "Accept".parse().unwrap());
    older_refresh_headers.insert("content-language", "en".parse().unwrap());
    let reply = settle_refresh_with_request_headers_before_settlement(
        &runtime,
        &token,
        source_id,
        source_url.as_str(),
        &source_ref,
        HopOutcome {
            status: 200,
            final_url: terminal_url.clone(),
            headers: older_refresh_headers,
            redirect_to: None,
            body: Some((BODY.to_vec(), false)),
        },
        &redirect_hops,
        &request_headers,
        async {
            let mut newer_get_headers = response_headers();
            newer_get_headers.insert("etag", "newer-get".parse().unwrap());
            newer_get_headers.insert("vary", "Accept-Language".parse().unwrap());
            newer_get_headers.insert("content-language", "fr".parse().unwrap());
            let fetched = settle_with_request_headers(
                &runtime,
                &token,
                "GET",
                &terminal_url,
                200,
                &newer_get_headers,
                Some((BODY.to_vec(), false)),
                &[],
                true,
                &[("Accept-Language".to_owned(), "fr".to_owned())],
            )
            .await
            .unwrap();
            assert_eq!(fetched["id"], terminal_id.to_string());
        },
    )
    .await
    .unwrap();

    assert_eq!(reply["lost_race"], true);
    let terminal = entity(&runtime, &token, terminal_id).await;
    let properties = terminal.properties.as_ref().unwrap();
    assert_eq!(properties["etag"], "newer-get");
    assert_eq!(properties["vary"], json!(["Accept-Language"]));
    assert_eq!(properties["content_language"], "fr");
    assert_eq!(
        properties["request_headers"],
        json!({"accept-language": ["fr"], "accept-encoding": ["identity"]})
    );
}

#[tokio::test]
async fn redirected_refresh_keeps_a_newer_get_body_after_the_terminal_request() {
    let (runtime, token, _dir) = fixture();
    let source_url = Url::parse("https://metadata.example/body-race-source").unwrap();
    let terminal_url = Url::parse("https://metadata.example/body-race-target").unwrap();
    let source_id = seed(&runtime, &token, &source_url, &[]).await;
    let terminal_id = seed(&runtime, &token, &terminal_url, &[]).await;
    let source_before = entity(&runtime, &token, source_id).await;
    let source_properties = source_before.properties.as_ref().unwrap();
    let source_ref = source_properties["blob_ref"].as_str().unwrap().to_owned();
    let request_headers = refresh_request_headers(source_properties).unwrap();
    let redirect_hops = [crate::fetch::RedirectHop {
        from: source_url.clone(),
        to: terminal_url.clone(),
        status: 302,
    }];

    let mut older_refresh_headers = response_headers();
    older_refresh_headers.insert("etag", "older-refresh-body".parse().unwrap());
    let reply = settle_refresh_with_request_headers_before_settlement(
        &runtime,
        &token,
        source_id,
        source_url.as_str(),
        &source_ref,
        HopOutcome {
            status: 200,
            final_url: terminal_url.clone(),
            headers: older_refresh_headers,
            redirect_to: None,
            body: Some((b"older refresh body".to_vec(), false)),
        },
        &redirect_hops,
        &request_headers,
        async {
            let mut newer_get_headers = response_headers();
            newer_get_headers.insert("etag", "newer-get-body".parse().unwrap());
            let fetched = settle_with_request_headers(
                &runtime,
                &token,
                "GET",
                &terminal_url,
                200,
                &newer_get_headers,
                Some((b"newer GET body".to_vec(), false)),
                &[],
                true,
                &[("Accept-Language".to_owned(), "fr".to_owned())],
            )
            .await
            .unwrap();
            assert_eq!(fetched["id"], terminal_id.to_string());
        },
    )
    .await
    .unwrap();

    assert_eq!(reply["lost_race"], true);
    assert_eq!(reply["changed"], false);
    let terminal = entity(&runtime, &token, terminal_id).await;
    let properties = terminal.properties.as_ref().unwrap();
    assert_eq!(properties["etag"], "newer-get-body");
    assert_eq!(
        properties["request_headers"],
        json!({"accept-language": ["fr"], "accept-encoding": ["identity"]})
    );
    let body_ref =
        khive_storage::ContentRef::from_hex(properties["blob_ref"].as_str().unwrap()).unwrap();
    let stored_body = crate::blob_store(&runtime)
        .unwrap()
        .get_bounded_verified(&body_ref, 64)
        .await
        .unwrap();
    assert_eq!(stored_body, b"newer GET body");
    let audit = receipt(&runtime, &token, &reply).await;
    assert_eq!(audit["content_ref"], Value::Null);
}

#[tokio::test]
async fn redirected_refresh_does_not_claim_a_terminal_row_created_during_its_request() {
    let (runtime, token, _dir) = fixture();
    let source_url = Url::parse("https://metadata.example/new-target-source").unwrap();
    let terminal_url = Url::parse("https://metadata.example/new-target").unwrap();
    let source_id = seed(&runtime, &token, &source_url, &[]).await;
    let terminal_id = document_id_for_url(&token, &terminal_url);
    assert!(runtime
        .entities(&token)
        .unwrap()
        .get_entity(terminal_id)
        .await
        .unwrap()
        .is_none());
    let source_before = entity(&runtime, &token, source_id).await;
    let source_properties = source_before.properties.as_ref().unwrap();
    let source_ref = source_properties["blob_ref"].as_str().unwrap().to_owned();
    let request_headers = refresh_request_headers(source_properties).unwrap();
    let redirect_hops = [crate::fetch::RedirectHop {
        from: source_url.clone(),
        to: terminal_url.clone(),
        status: 302,
    }];

    let reply = settle_refresh_with_request_headers_before_settlement(
        &runtime,
        &token,
        source_id,
        source_url.as_str(),
        &source_ref,
        HopOutcome {
            status: 200,
            final_url: terminal_url.clone(),
            headers: response_headers(),
            redirect_to: None,
            body: Some((b"older refresh body".to_vec(), false)),
        },
        &redirect_hops,
        &request_headers,
        async {
            let mut newer_get_headers = response_headers();
            newer_get_headers.insert("etag", "newly-created-get".parse().unwrap());
            settle_with_request_headers(
                &runtime,
                &token,
                "GET",
                &terminal_url,
                200,
                &newer_get_headers,
                Some((b"newly-created GET body".to_vec(), false)),
                &[],
                true,
                &[],
            )
            .await
            .unwrap();
        },
    )
    .await
    .unwrap();

    assert_eq!(reply["lost_race"], true);
    assert_eq!(reply["changed"], false);
    let terminal = entity(&runtime, &token, terminal_id).await;
    let properties = terminal.properties.as_ref().unwrap();
    assert_eq!(properties["etag"], "newly-created-get");
    let body_ref =
        khive_storage::ContentRef::from_hex(properties["blob_ref"].as_str().unwrap()).unwrap();
    let stored_body = crate::blob_store(&runtime)
        .unwrap()
        .get_bounded_verified(&body_ref, 64)
        .await
        .unwrap();
    assert_eq!(stored_body, b"newly-created GET body");
}

#[tokio::test]
async fn run_refresh_redirect_loses_to_get_after_terminal_request() {
    use crate::egress::resolver_fixture::ScriptedResolver;

    async fn request_line(socket: &mut tokio::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let byte = socket.read_u8().await.expect("read HTTP request");
            bytes.push(byte);
            assert!(bytes.len() < 16_384, "request headers exceeded test bound");
        }
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned()
    }

    let (runtime, token, _dir) = fixture();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let source_url =
        Url::parse(&format!("http://refresh-race.example.test:{port}/source")).unwrap();
    let terminal_url =
        Url::parse(&format!("http://refresh-race.example.test:{port}/terminal")).unwrap();
    let source_id = seed(&runtime, &token, &source_url, &[]).await;
    let terminal_id = seed(&runtime, &token, &terminal_url, &[]).await;
    let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server_terminal_url = terminal_url.clone();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        assert_eq!(request_line(&mut first).await, "GET /source HTTP/1.1");
        first
            .write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {server_terminal_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        first.shutdown().await.unwrap();
        drop(first);

        let (mut terminal, _) = listener.accept().await.unwrap();
        assert_eq!(request_line(&mut terminal).await, "GET /terminal HTTP/1.1");
        request_seen_tx.send(()).unwrap();
        release_rx.await.expect("release older refresh response");
        let body = b"older refresh body";
        terminal
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nETag: older-refresh\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        terminal.write_all(body).await.unwrap();
        terminal.shutdown().await.unwrap();
    });

    // Policy still validates a public DNS answer. Only this test's checked
    // transport maps that host to the local paused HTTP server.
    let resolver = ScriptedResolver::new(None);
    let clients = crate::egress::PinnedClients::default();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(
            "refresh-race.example.test",
            std::net::SocketAddr::new("127.0.0.1".parse().unwrap(), port),
        )
        .build()
        .unwrap();
    clients.insert_for_test(&source_url, resolver.address, client);
    let run_runtime = runtime.clone();
    let run_token = token.clone();
    let run = tokio::spawn(async move {
        let params = serde_json::from_value(json!({"id": source_id, "timeout_s": 5})).unwrap();
        run_refresh(
            &run_runtime,
            &run_token,
            &resolver,
            &Default::default(),
            params,
            &clients,
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(5), request_seen_rx)
        .await
        .expect("production refresh did not reach the terminal request")
        .unwrap();
    let mut newer_headers = response_headers();
    newer_headers.insert("etag", "newer-get".parse().unwrap());
    let newer = settle_with_request_headers(
        &runtime,
        &token,
        "GET",
        &terminal_url,
        200,
        &newer_headers,
        Some((b"newer GET body".to_vec(), false)),
        &[],
        true,
        &[],
    )
    .await
    .unwrap();
    assert_eq!(newer["id"], terminal_id.to_string());
    release_tx.send(()).unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("refresh did not settle")
        .unwrap()
        .unwrap();
    server.await.unwrap();

    assert_eq!(reply["lost_race"], true, "{reply}");
    assert_eq!(reply["changed"], false, "{reply}");
    let terminal = entity(&runtime, &token, terminal_id).await;
    let properties = terminal.properties.as_ref().unwrap();
    assert_eq!(properties["etag"], "newer-get");
    let body_ref =
        khive_storage::ContentRef::from_hex(properties["blob_ref"].as_str().unwrap()).unwrap();
    let body = crate::blob_store(&runtime)
        .unwrap()
        .get_bounded_verified(&body_ref, 64)
        .await
        .unwrap();
    assert_eq!(body, b"newer GET body");
}

// Simulate a fetch paused between its body settlement and negotiation write.
#[tokio::test]
async fn overlapping_fetch_keeps_negotiation_with_the_stored_body() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://metadata.example/fetch-overlap").unwrap();
    let first_headers = vec![("Accept-Language".to_owned(), "fr".to_owned())];
    let second_headers = vec![("Accept-Language".to_owned(), "en".to_owned())];
    let first = crate::fetch::settle_content(
        &runtime,
        &token,
        &url,
        Some("text/plain"),
        200,
        Some("old-etag"),
        Some(MODIFIED),
        Some((b"variant A".to_vec(), false)),
    )
    .await
    .unwrap();
    let mut response_b = response_headers();
    response_b.insert("etag", "etag-b".parse().unwrap());
    let second = settle_with_request_headers(
        &runtime,
        &token,
        "GET",
        &url,
        200,
        &response_b,
        Some((b"variant B".to_vec(), false)),
        &[],
        true,
        &second_headers,
    )
    .await
    .unwrap();
    let stale = crate::fetch::persist_get_context(
        &runtime,
        &token,
        &first,
        &first_headers,
        &response_headers(),
    )
    .await
    .expect_err("a stale fetch must not write context onto a newer body");
    assert_eq!(
        khive_runtime::runtime_error_value(stale, khive_runtime::DomainDisposition::Unknown)
            ["kind"],
        "conflict"
    );
    let after = entity(&runtime, &token, first.id).await;
    let properties = after.properties.unwrap();
    assert_eq!(properties["blob_ref"], second["content_ref"]);
    assert_eq!(properties["etag"], "etag-b");
    let sent = refresh_request_headers(&properties).unwrap();
    assert_eq!(
        crate::fetch::negotiation_headers(&sent),
        crate::fetch::recorded_negotiation_headers(&second_headers),
        "stored negotiation must describe the stored body, not the last metadata-only writer"
    );
}

#[tokio::test]
async fn fetch_records_repeated_vary_and_language_while_head_preserves_get_context() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://metadata.example/vary-fetch").unwrap();
    let sent = vec![
        ("Accept".to_owned(), "text/plain".to_owned()),
        ("Accept-Language".to_owned(), "fr-CA".to_owned()),
    ];
    let mut headers = response_headers();
    headers.append("vary", "Accept".parse().unwrap());
    headers.append("vary", "Accept-Language".parse().unwrap());
    headers.insert("content-language", "fr-CA".parse().unwrap());
    let get = settle_with_request_headers(
        &runtime,
        &token,
        "GET",
        &url,
        200,
        &headers,
        Some((BODY.to_vec(), false)),
        &[],
        true,
        &sent,
    )
    .await
    .unwrap();
    let id = Uuid::parse_str(get["id"].as_str().unwrap()).unwrap();
    let before = entity(&runtime, &token, id).await;
    let properties = before.properties.as_ref().unwrap();
    assert_eq!(properties["vary"], json!(["Accept", "Accept-Language"]));
    assert_eq!(properties["content_language"], "fr-CA");
    let get_receipt = receipt(&runtime, &token, &get).await;
    assert_eq!(get_receipt["headers"]["vary"], properties["vary"]);
    assert_eq!(get_receipt["headers"]["content-language"], "fr-CA");
    assert_eq!(refresh_request_headers(properties).unwrap().len(), 5);

    let mut head_headers = HeaderMap::new();
    head_headers.insert("vary", "*".parse().unwrap());
    head_headers.insert("content-language", "en".parse().unwrap());
    let head = settle_with_request_headers(
        &runtime,
        &token,
        "HEAD",
        &url,
        200,
        &head_headers,
        None,
        &[],
        true,
        &[("Accept-Language".to_owned(), "en".to_owned())],
    )
    .await
    .unwrap();
    let after = entity(&runtime, &token, id).await;
    let properties = after.properties.as_ref().unwrap();
    assert_eq!(properties["vary"], json!(["Accept", "Accept-Language"]));
    assert_eq!(properties["content_language"], "fr-CA");
    assert_eq!(
        properties["request_headers"],
        before.properties.unwrap()["request_headers"]
    );
    let head_receipt = receipt(&runtime, &token, &head).await;
    assert_eq!(head_receipt["headers"]["vary"], json!(["*"]));
    assert_eq!(head_receipt["headers"]["content-language"], "en");
}

#[test]
fn vary_gate_requires_every_stored_selector_and_value() {
    let url = Url::parse("https://metadata.example/vary-gate").unwrap();
    let base = json!({
        "url": url.as_str(),
        "etag": "old-etag",
        "truncated": false,
        "request_headers": {
            "accept": ["text/plain"],
            "accept-language": ["fr-CA"],
            "accept-encoding": ["identity"]
        }
    });
    for (vary, replayable) in [
        (json!([]), true),
        (json!(["Accept, Accept-Language"]), true),
        (json!(["Accept", "Accept-Language"]), true),
        (json!(["*"]), false),
        (json!(["Accept-Encoding"]), true),
        (json!(["Accept,"]), false),
        (json!([null]), false),
    ] {
        let mut properties = base.clone();
        properties["vary"] = vary;
        assert_eq!(
            !conditional_headers_for_hop(&properties, &url, &url, true).is_empty(),
            replayable,
            "vary={}",
            properties["vary"]
        );
    }
    let mut legacy = base.clone();
    legacy.as_object_mut().unwrap().remove("vary");
    assert!(conditional_headers_for_hop(&legacy, &url, &url, true).is_empty());
    let mut missing_map = base.clone();
    missing_map["vary"] = json!([]);
    missing_map
        .as_object_mut()
        .unwrap()
        .remove("request_headers");
    assert!(conditional_headers_for_hop(&missing_map, &url, &url, true).is_empty());
    let mut malformed_map = base.clone();
    malformed_map["vary"] = json!([]);
    malformed_map["request_headers"] = json!("not an object");
    assert!(conditional_headers_for_hop(&malformed_map, &url, &url, true).is_empty());
    let mut unknown_map_key = base.clone();
    unknown_map_key["vary"] = json!([]);
    unknown_map_key["request_headers"]["user-agent"] = json!(["khive"]);
    assert!(conditional_headers_for_hop(&unknown_map_key, &url, &url, true).is_empty());
    let mut invalid_value = base.clone();
    invalid_value["vary"] = json!(["Accept"]);
    invalid_value["request_headers"]["accept"] = json!(["text/plain\r\ninvalid"]);
    assert!(conditional_headers_for_hop(&invalid_value, &url, &url, true).is_empty());
    let mut missing_encoding = base.clone();
    missing_encoding["vary"] = json!(["Accept-Encoding"]);
    missing_encoding["request_headers"]
        .as_object_mut()
        .unwrap()
        .remove("accept-encoding");
    assert!(conditional_headers_for_hop(&missing_encoding, &url, &url, true).is_empty());
    let mut wrong_encoding = base.clone();
    wrong_encoding["vary"] = json!(["Accept-Encoding"]);
    wrong_encoding["request_headers"]["accept-encoding"] = json!(["br"]);
    assert!(conditional_headers_for_hop(&wrong_encoding, &url, &url, true).is_empty());
    let mut missing = base;
    missing["vary"] = json!(["Accept-Language"]);
    missing["request_headers"]["accept-language"] = json!([]);
    assert!(conditional_headers_for_hop(&missing, &url, &url, true).is_empty());
}

#[test]
fn vary_accept_encoding_with_fixed_record_sends_validator() {
    let url = Url::parse("https://metadata.example/encoding-validator").unwrap();
    let properties = json!({
        "url": url.as_str(),
        "etag": "encoded-etag",
        "truncated": false,
        "vary": ["Accept-Encoding"],
        "request_headers": {"accept-encoding": ["identity"]}
    });
    let sent = refresh_request_headers(&properties).unwrap();
    assert!(sent.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("accept-encoding") && value == "identity"
    }));
    assert!(sent.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("if-none-match") && value == "encoded-etag"
    }));
}

#[test]
fn stored_gzip_encoding_map_replays_identity_without_validators() {
    let url = Url::parse("https://metadata.example/stored-gzip-map").unwrap();
    let properties = json!({
        "url": url.as_str(),
        "etag": "stored-etag",
        "truncated": false,
        "vary": ["Accept, Accept-Encoding"],
        "request_headers": {
            "accept": ["text/plain"],
            "accept-encoding": ["gzip"]
        }
    });
    let sent = refresh_request_headers(&properties).unwrap();
    assert_eq!(
        crate::fetch::negotiation_headers(&sent),
        crate::fetch::negotiation_headers(&[
            ("accept".to_string(), "text/plain".to_string()),
            ("accept-encoding".to_string(), "identity".to_string()),
        ]),
        "an earlier record keeps its replayed fields and never resends gzip"
    );
    assert!(sent.iter().all(|(name, _)| {
        !name.eq_ignore_ascii_case("if-none-match")
            && !name.eq_ignore_ascii_case("if-modified-since")
    }));
}

#[tokio::test]
async fn legacy_gzip_body_requires_identity_get_before_validation() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://metadata.example/legacy-gzip-body").unwrap();
    let id = seed(&runtime, &token, &url, &[]).await;
    crate::entities::patch(
        &runtime,
        &token,
        id,
        None,
        json!({
            "vary": ["Accept-Encoding"],
            "request_headers": {"accept-encoding": ["gzip"]}
        }),
    )
    .await
    .unwrap();
    let before = entity(&runtime, &token, id).await;
    let properties = before.properties.as_ref().unwrap();
    let sent = refresh_request_headers(properties).unwrap();
    assert_eq!(
        crate::fetch::negotiation_headers(&sent)["accept-encoding"],
        vec!["identity".to_string()]
    );
    assert!(sent.iter().all(|(name, _)| {
        !name.eq_ignore_ascii_case("if-none-match")
            && !name.eq_ignore_ascii_case("if-modified-since")
    }));

    let no_body = settle_refresh_with_request_headers(
        &runtime,
        &token,
        id,
        url.as_str(),
        properties["blob_ref"].as_str().unwrap(),
        HopOutcome {
            status: 304,
            final_url: url.clone(),
            headers: HeaderMap::new(),
            redirect_to: None,
            body: Some((vec![], false)),
        },
        &[],
        &sent,
    )
    .await;
    assert!(
        no_body
            .unwrap_err()
            .to_string()
            .contains("unsolicited_not_modified"),
        "an unsolicited 304 cannot bless the legacy body"
    );
    assert_eq!(
        serde_json::to_value(entity(&runtime, &token, id).await).unwrap(),
        serde_json::to_value(&before).unwrap(),
        "a bodyless reply cannot change the legacy row"
    );
    assert_eq!(
        entity(&runtime, &token, id).await.properties.unwrap()["request_headers"]
            ["accept-encoding"],
        json!(["gzip"])
    );

    refresh(&runtime, &token, id, &url, 200, response_headers(), &[]).await;
    let after = entity(&runtime, &token, id).await;
    let properties = after.properties.unwrap();
    assert_eq!(
        properties["request_headers"]["accept-encoding"],
        json!(["identity"])
    );
    let next = refresh_request_headers(&properties).unwrap();
    assert!(next.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("if-none-match") && value == "old-etag"
    }));
}

#[tokio::test]
async fn represented_accept_encoding_on_304_updates_vary() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://metadata.example/encoding-304").unwrap();
    let id = seed(&runtime, &token, &url, &[]).await;
    let mut headers = HeaderMap::new();
    headers.insert("vary", "Accept-Encoding".parse().unwrap());
    let reply = refresh(&runtime, &token, id, &url, 304, headers, &[]).await;
    assert_eq!(reply["changed"], false);
    let after = entity(&runtime, &token, id).await;
    assert_eq!(
        after.properties.unwrap()["vary"],
        json!(["Accept-Encoding"])
    );
}

#[tokio::test]
async fn legacy_map_304_represents_wire_identity_but_next_refresh_is_unconditional() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://metadata.example/legacy-encoding-304").unwrap();
    let id = seed(&runtime, &token, &url, &[]).await;
    crate::entities::patch(
        &runtime,
        &token,
        id,
        None,
        json!({"request_headers": {}, "vary": []}),
    )
    .await
    .unwrap();
    let before = entity(&runtime, &token, id).await;
    let sent = refresh_request_headers(before.properties.as_ref().unwrap()).unwrap();
    assert!(sent.iter().any(|(name, _)| name == "If-None-Match"));
    assert!(!sent
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("accept-encoding")));

    let mut headers = HeaderMap::new();
    headers.insert("vary", "Accept-Encoding".parse().unwrap());
    let reply = refresh(&runtime, &token, id, &url, 304, headers, &[]).await;
    assert_eq!(reply["changed"], false);
    let after = entity(&runtime, &token, id).await;
    let properties = after.properties.unwrap();
    assert_eq!(properties["request_headers"], json!({}));
    assert_eq!(properties["vary"], json!(["Accept-Encoding"]));
    assert!(refresh_request_headers(&properties)
        .unwrap()
        .iter()
        .all(|(name, _)| name != "If-None-Match"));
}

#[test]
fn response_header_projection_keeps_repeats_and_invalid_vary_visible() {
    let mut headers = HeaderMap::new();
    headers.append("vary", "Accept".parse().unwrap());
    headers.append(
        "vary",
        reqwest::header::HeaderValue::from_bytes(b"\xff").unwrap(),
    );
    headers.append("content-language", "fr".parse().unwrap());
    headers.append("content-language", "en".parse().unwrap());
    let projected = crate::fetch::extract_allowed_headers(&headers);
    assert_eq!(projected["vary"], json!(["Accept", null]));
    assert_eq!(projected["content-language"], "fr, en");
    assert!(!vary_is_replayable(
        &projected["vary"],
        &[("Accept".to_owned(), "text/plain".to_owned())]
    ));
    headers.append(
        "content-language",
        reqwest::header::HeaderValue::from_bytes(b"\xff").unwrap(),
    );
    assert!(crate::fetch::extract_allowed_headers(&headers)["content-language"].is_null());
}

#[tokio::test]
async fn malformed_cached_negotiation_stays_unconditional_across_body_refreshes() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://metadata.example/malformed-negotiation").unwrap();
    let id = seed(
        &runtime,
        &token,
        &url,
        &[("Accept-Language".to_owned(), "fr".to_owned())],
    )
    .await;
    crate::entities::patch(
        &runtime,
        &token,
        id,
        None,
        json!({ "vary": ["Accept-Language"], "request_headers": {"accept-language": "fr"} }),
    )
    .await
    .unwrap();
    let before = entity(&runtime, &token, id).await;
    let properties = before.properties.as_ref().unwrap();
    let sent = refresh_request_headers(properties).unwrap();
    assert!(
        sent.is_empty(),
        "invalid cached negotiation cannot be replayed"
    );
    let reply = settle_refresh_with_request_headers(
        &runtime,
        &token,
        id,
        url.as_str(),
        properties["blob_ref"].as_str().unwrap(),
        HopOutcome {
            status: 200,
            final_url: url.clone(),
            headers: response_headers(),
            redirect_to: None,
            body: Some((BODY.to_vec(), false)),
        },
        &[],
        &sent,
    )
    .await
    .unwrap();
    assert_eq!(reply["changed"], false);
    let after = entity(&runtime, &token, id).await;
    let properties = after.properties.unwrap();
    assert_eq!(
        properties["request_headers"],
        json!({"accept-language": "fr"})
    );
    assert_eq!(properties["vary"], json!([]));
    assert!(refresh_request_headers(&properties).unwrap().is_empty());

    let second = refresh(&runtime, &token, id, &url, 200, response_headers(), &[]).await;
    assert_eq!(second["changed"], false);
    let after_second = entity(&runtime, &token, id).await;
    let properties = after_second.properties.unwrap();
    assert_eq!(
        properties["request_headers"],
        json!({"accept-language": "fr"})
    );
    assert_eq!(properties["vary"], json!([]));
    assert!(refresh_request_headers(&properties).unwrap().is_empty());
}

#[tokio::test]
async fn same_identity_redirect_does_not_replace_source_negotiation() {
    let (runtime, token, _dir) = fixture();
    let source = Url::parse("https://metadata.example/same?z=1&a=2").unwrap();
    let terminal = Url::parse("https://metadata.example/same?a=2&z=1").unwrap();
    let id = seed(
        &runtime,
        &token,
        &source,
        &[("Accept-Language".to_owned(), "fr".to_owned())],
    )
    .await;
    crate::entities::patch(
        &runtime,
        &token,
        id,
        None,
        json!({"request_headers": {"accept-language": "fr"}}),
    )
    .await
    .unwrap();
    let before = entity(&runtime, &token, id).await;
    let properties = before.properties.as_ref().unwrap();
    let sent = refresh_request_headers(properties).unwrap();
    assert!(sent.is_empty(), "malformed source map cannot be replayed");

    let reply = settle_refresh_with_request_headers(
        &runtime,
        &token,
        id,
        source.as_str(),
        properties["blob_ref"].as_str().unwrap(),
        HopOutcome {
            status: 200,
            final_url: terminal.clone(),
            headers: response_headers(),
            redirect_to: None,
            body: Some((BODY.to_vec(), false)),
        },
        &[crate::fetch::RedirectHop {
            from: source.clone(),
            to: terminal,
            status: 302,
        }],
        &sent,
    )
    .await
    .unwrap();
    assert_eq!(reply["final_id"], id.to_string());
    let after = entity(&runtime, &token, id).await;
    assert_eq!(
        after.properties.as_ref().unwrap()["request_headers"],
        json!({"accept-language": "fr"}),
        "a same-identity redirect still addresses the source row"
    );
}

#[tokio::test]
async fn concurrent_get_keeps_its_negotiation_after_same_body_refresh() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://metadata.example/refresh-get-overlap").unwrap();
    let id = seed(
        &runtime,
        &token,
        &url,
        &[("Accept-Language".to_owned(), "fr".to_owned())],
    )
    .await;
    let before = entity(&runtime, &token, id).await;
    let properties = before.properties.as_ref().unwrap();
    let original_ref = properties["blob_ref"].as_str().unwrap().to_owned();
    let sent = refresh_request_headers(properties).unwrap();

    let (settled_tx, settled_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let refresh_runtime = runtime.clone();
    let refresh_token = token.clone();
    let refresh_url = url.clone();
    let paused = tokio::spawn(async move {
        settle_refresh_with_request_headers_after_body_settlement(
            &refresh_runtime,
            &refresh_token,
            id,
            refresh_url.as_str(),
            &original_ref,
            HopOutcome {
                status: 200,
                final_url: refresh_url.clone(),
                headers: response_headers(),
                redirect_to: None,
                body: Some((BODY.to_vec(), false)),
            },
            &[],
            &sent,
            async move {
                settled_tx
                    .send(())
                    .expect("GET waits for refresh settlement");
                resume_rx.await.expect("resume paused refresh");
            },
        )
        .await
    });
    settled_rx.await.expect("refresh body settled");

    settle_with_request_headers(
        &runtime,
        &token,
        "GET",
        &url,
        200,
        &response_headers(),
        Some((BODY.to_vec(), false)),
        &[],
        true,
        &[("Accept-Language".to_owned(), "en".to_owned())],
    )
    .await
    .expect("later caller GET");
    let after_get = entity(&runtime, &token, id).await;
    assert_eq!(
        after_get.properties.as_ref().unwrap()["request_headers"],
        json!({"accept-language": ["en"], "accept-encoding": ["identity"]})
    );
    assert_eq!(
        after_get.properties.as_ref().unwrap()["blob_ref"],
        properties["blob_ref"],
        "the GET has the same body so the refresh metadata guard still matches"
    );

    resume_tx.send(()).expect("release refresh");
    paused.await.unwrap().unwrap();
    let final_entity = entity(&runtime, &token, id).await;
    assert_eq!(
        final_entity.properties.unwrap()["request_headers"],
        json!({"accept-language": ["en"], "accept-encoding": ["identity"]}),
        "A5 allows only the caller's GET to replace stored negotiation"
    );
}

#[tokio::test]
async fn refresh_replaces_vary_and_language_on_200_but_304_updates_only_supplied() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://metadata.example/vary-refresh").unwrap();
    let sent = vec![("Accept-Language".to_owned(), "fr-CA".to_owned())];
    let mut initial = response_headers();
    initial.insert("vary", "Accept-Language".parse().unwrap());
    initial.insert("content-language", "fr-CA".parse().unwrap());
    let get = settle_with_request_headers(
        &runtime,
        &token,
        "GET",
        &url,
        200,
        &initial,
        Some((BODY.to_vec(), false)),
        &[],
        true,
        &sent,
    )
    .await
    .unwrap();
    let id = Uuid::parse_str(get["id"].as_str().unwrap()).unwrap();
    let full = refresh(&runtime, &token, id, &url, 200, response_headers(), &[]).await;
    let after_full = entity(&runtime, &token, id).await;
    let properties = after_full.properties.as_ref().unwrap();
    assert_eq!(properties["vary"], json!([]));
    assert!(properties["content_language"].is_null());
    assert!(receipt(&runtime, &token, &full).await["headers"]
        .get("vary")
        .is_none());

    let mut validation = HeaderMap::new();
    validation.insert("vary", "Accept-Language".parse().unwrap());
    validation.insert("content-language", "de".parse().unwrap());
    let fresh = refresh(&runtime, &token, id, &url, 304, validation, &[]).await;
    let after_fresh = entity(&runtime, &token, id).await;
    let properties = after_fresh.properties.as_ref().unwrap();
    assert_eq!(properties["vary"], json!(["Accept-Language"]));
    assert_eq!(properties["content_language"], "de");
    assert_eq!(fresh["changed"], false);
    let unchanged = refresh(&runtime, &token, id, &url, 304, HeaderMap::new(), &[]).await;
    let after_unchanged = entity(&runtime, &token, id).await;
    let properties = after_unchanged.properties.unwrap();
    assert_eq!(properties["vary"], json!(["Accept-Language"]));
    assert_eq!(properties["content_language"], "de");
    assert_eq!(receipt(&runtime, &token, &unchanged).await["status"], 304);
}

#[tokio::test]
async fn changed_body_refresh_rebinds_response_selection_context() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://metadata.example/changed-vary").unwrap();
    let sent = vec![("Accept-Language".to_owned(), "fr".to_owned())];
    let mut initial = response_headers();
    initial.insert("vary", "Accept-Language".parse().unwrap());
    initial.insert("content-language", "fr".parse().unwrap());
    let get = settle_with_request_headers(
        &runtime,
        &token,
        "GET",
        &url,
        200,
        &initial,
        Some((BODY.to_vec(), false)),
        &[],
        true,
        &sent,
    )
    .await
    .unwrap();
    let id = Uuid::parse_str(get["id"].as_str().unwrap()).unwrap();
    let before = entity(&runtime, &token, id).await;
    let request_headers = refresh_request_headers(before.properties.as_ref().unwrap()).unwrap();
    let reply = settle_refresh_with_request_headers(
        &runtime,
        &token,
        id,
        url.as_str(),
        before.properties.as_ref().unwrap()["blob_ref"]
            .as_str()
            .unwrap(),
        HopOutcome {
            status: 200,
            final_url: url.clone(),
            headers: response_headers(),
            redirect_to: None,
            body: Some((b"different representation".to_vec(), false)),
        },
        &[],
        &request_headers,
    )
    .await
    .unwrap();
    assert_eq!(reply["changed"], true);
    let after = entity(&runtime, &token, id).await;
    let properties = after.properties.as_ref().unwrap();
    assert_ne!(
        properties["blob_ref"],
        before.properties.as_ref().unwrap()["blob_ref"]
    );
    assert_eq!(properties["vary"], json!([]));
    assert!(properties["content_language"].is_null());
    assert_eq!(
        properties["request_headers"],
        before.properties.unwrap()["request_headers"]
    );
}

#[tokio::test]
async fn unsolicited_or_unrepresented_304_refuses_before_any_write() {
    for case in [
        "no-validator",
        "unknown-vary",
        "repeated-vary",
        "invalid-utf8",
    ] {
        let (runtime, token, _dir) = fixture();
        let url = Url::parse("https://metadata.example/vary-304-refusal").unwrap();
        let sent = vec![("Accept-Language".to_owned(), "fr".to_owned())];
        let mut initial = response_headers();
        initial.insert("vary", "Accept-Language".parse().unwrap());
        let get = settle_with_request_headers(
            &runtime,
            &token,
            "GET",
            &url,
            200,
            &initial,
            Some((BODY.to_vec(), false)),
            &[],
            true,
            &sent,
        )
        .await
        .unwrap();
        let id = Uuid::parse_str(get["id"].as_str().unwrap()).unwrap();
        let before = entity(&runtime, &token, id).await;
        let before_notes = runtime
            .notes(&token)
            .unwrap()
            .count_notes(token.namespace().as_str(), None)
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        match case {
            "unknown-vary" => {
                headers.insert("vary", "X-Variant".parse().unwrap());
            }
            "repeated-vary" => {
                headers.append("vary", "Accept-Language".parse().unwrap());
                headers.append("vary", "User-Agent".parse().unwrap());
            }
            "invalid-utf8" => {
                headers.insert(
                    "vary",
                    reqwest::header::HeaderValue::from_bytes(b"Accept-Language,\xff").unwrap(),
                );
            }
            _ => {}
        }
        let request_headers = if case == "no-validator" {
            sent.clone()
        } else {
            refresh_request_headers(before.properties.as_ref().unwrap()).unwrap()
        };
        let error = settle_refresh_with_request_headers(
            &runtime,
            &token,
            id,
            url.as_str(),
            before.properties.as_ref().unwrap()["blob_ref"]
                .as_str()
                .unwrap(),
            HopOutcome {
                status: 304,
                final_url: url.clone(),
                headers,
                redirect_to: None,
                body: None,
            },
            &[],
            &request_headers,
        )
        .await
        .expect_err("an unvalidated representation must not accept 304 metadata");
        assert!(
            error.to_string().contains(if case == "no-validator" {
                "unsolicited_not_modified"
            } else {
                "unrepresented_vary"
            }),
            "case={case}: {error}"
        );
        assert_eq!(
            serde_json::to_value(entity(&runtime, &token, id).await).unwrap(),
            serde_json::to_value(before).unwrap(),
            "case={case}"
        );
        assert_eq!(
            runtime
                .notes(&token)
                .unwrap()
                .count_notes(token.namespace().as_str(), None)
                .await
                .unwrap(),
            before_notes,
            "case={case}"
        );
    }
}

mod refresh_revision_tests {
    use super::*;

    const NEUTRAL_BODY: &[u8] = br#"{"message":"neutral"}"#;

    async fn put_variant(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        url: &Url,
        language: &str,
        etag: &str,
        body: &[u8],
    ) -> Value {
        let mut response = HeaderMap::new();
        response.insert("content-type", "application/json".parse().unwrap());
        response.insert("etag", etag.parse().unwrap());
        let sent = vec![("Accept-Language".to_owned(), language.to_owned())];
        settle_with_request_headers(
            runtime,
            token,
            "GET",
            url,
            200,
            &response,
            Some((body.to_vec(), false)),
            &[],
            true,
            &sent,
        )
        .await
        .expect("synthetic persisted GET must settle")
    }

    fn assert_winner(row: &Entity, language: &str, expected_ref: &str) {
        let properties = row.properties.as_ref().unwrap();
        assert_eq!(properties["blob_ref"], expected_ref);
        assert_eq!(row.content_ref.as_deref(), Some(expected_ref));
        assert_eq!(
            properties["request_headers"],
            json!({"accept-language": [language], "accept-encoding": ["identity"]}),
            "a refresh must not replace the later caller-issued GET negotiation"
        );
        assert_eq!(
            properties["etag"], "\"get-b\"",
            "an old validation response must not replace the winner's validator"
        );
        assert_eq!(properties["content_type"], "application/json");
        assert_eq!(properties["truncated"], false);
        assert_eq!(properties["status"], 200);
    }

    async fn late_response_case(
        status: u16,
        supplies_metadata: bool,
        publish_b_after_body_phase: bool,
        winner_language: &str,
    ) {
        let (runtime, token, _dir) = fixture();
        let url = Url::parse("https://review.example/same-bytes").unwrap();
        let first = put_variant(&runtime, &token, &url, "fr", "\"get-a\"", NEUTRAL_BODY).await;
        let id = Uuid::parse_str(first["id"].as_str().unwrap()).unwrap();
        let before = entity(&runtime, &token, id).await;
        let old = before.properties.as_ref().unwrap();
        let expected_ref = old["blob_ref"].as_str().unwrap().to_owned();
        let captured_request = refresh_request_headers(old).unwrap();

        if !publish_b_after_body_phase {
            let second = put_variant(
                &runtime,
                &token,
                &url,
                winner_language,
                "\"get-b\"",
                NEUTRAL_BODY,
            )
            .await;
            assert_eq!(
                second["content_ref"], expected_ref,
                "identical byte precondition"
            );
            assert_winner(
                &entity(&runtime, &token, id).await,
                winner_language,
                &expected_ref,
            );
        }

        let mut response = HeaderMap::new();
        if supplies_metadata {
            // A normal old response: its entity tag matches A's request,
            // not the later GET's validator. It need not describe new bytes.
            response.insert("etag", "\"get-a\"".parse().unwrap());
            if status != 304 {
                response.insert("content-type", "text/plain".parse().unwrap());
            }
        }
        let outcome = HopOutcome {
            status,
            final_url: url.clone(),
            headers: response,
            redirect_to: None,
            body: if status == 304 {
                None
            } else {
                Some((NEUTRAL_BODY.to_vec(), false))
            },
        };
        let result = settle_refresh_from_snapshot(
            &runtime,
            &token,
            &before,
            None,
            id,
            url.as_str(),
            &expected_ref,
            outcome,
            &[],
            &captured_request,
            async {
                if publish_b_after_body_phase {
                    let second = put_variant(
                        &runtime,
                        &token,
                        &url,
                        winner_language,
                        "\"get-b\"",
                        NEUTRAL_BODY,
                    )
                    .await;
                    assert_eq!(second["content_ref"], expected_ref);
                    assert_winner(
                        &entity(&runtime, &token, id).await,
                        winner_language,
                        &expected_ref,
                    );
                }
            },
        )
        .await;

        // Check state even when an implementation chooses a typed refusal.
        assert_winner(
            &entity(&runtime, &token, id).await,
            winner_language,
            &expected_ref,
        );
        match result {
            Ok(reply) => {
                assert_eq!(reply["lost_race"], true);
                let audit = receipt(&runtime, &token, &reply).await;
                assert_eq!(audit["lost_race"], true);
                assert_eq!(
                    audit["request_headers"],
                    json!({"accept-language": ["fr"], "accept-encoding": ["identity"]}),
                    "receipt must report A's actual request, not B's saved context"
                );
            }
            Err(RuntimeError::Khive(error)) if error.kind() == khive_types::ErrorKind::Conflict => {
            }
            Err(error) => {
                // A repair may choose a named pre-settlement refusal instead.
                let text = error.to_string();
                assert!(
                    text.contains("cached_representation_changed")
                        || text.contains("cached_body_changed"),
                    "unrelated setup/storage errors are not a successful refusal: {text}"
                );
            }
        }
    }

    #[tokio::test]
    async fn stale_304_cannot_restore_old_get_context_for_equal_bytes() {
        late_response_case(304, true, false, "en").await;
    }

    #[tokio::test]
    async fn headerless_304_cannot_rewrite_get_owned_negotiation() {
        // No replacement headers still cannot validate against a stale row.
        late_response_case(304, false, false, "en").await;
    }

    #[tokio::test]
    async fn stale_same_body_200_cannot_restore_old_get_context() {
        late_response_case(200, true, false, "en").await;
    }

    #[tokio::test]
    async fn same_body_get_between_settlement_and_metadata_is_detected() {
        late_response_case(304, true, true, "en").await;
    }

    #[tokio::test]
    async fn same_negotiation_but_new_validator_still_needs_a_revision_fence() {
        // Identical negotiation does not make the old validator current.
        late_response_case(304, true, false, "fr").await;
    }
}
