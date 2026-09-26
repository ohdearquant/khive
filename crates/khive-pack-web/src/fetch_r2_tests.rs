//! Settlement and preflight regressions use the production fetch seams.
use super::*;
use khive_runtime::{Namespace, VerbRegistryBuilder};
use khive_storage::{Direction, EdgeRelation};
use std::sync::Arc;

async fn fixture() -> (KhiveRuntime, NamespaceToken, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0).unwrap();
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(WebPack::new(runtime.clone()));
    runtime.install_edge_rules(builder.build().unwrap().all_edge_rules());
    runtime.install_blob_store(Arc::new(store)).unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    (runtime, token, dir)
}

fn document_id(url: &Url) -> Uuid {
    let canonical = crate::identity::canonicalize(url.clone());
    crate::identity::document_id(
        crate::identity::site_id(&canonical),
        &crate::identity::path_and_query(&canonical),
    )
}

// Must fail if temporary hops unconditionally mint or patch either endpoint.
#[tokio::test]
async fn temporary_hops_do_not_mint_endpoints_or_patch_existing_source() {
    for status in [302, 307] {
        let (runtime, token, _dir) = fixture().await;
        let from = Url::parse("https://temporary-source.example/start").unwrap();
        let to = Url::parse("https://temporary-target.example/end").unwrap();
        let hop = RedirectHop {
            from: from.clone(),
            to: to.clone(),
            status,
        };
        assert!(
            settle_redirect_hops(&runtime, &token, std::slice::from_ref(&hop))
                .await
                .unwrap()
                .is_empty()
        );
        for url in [&from, &to] {
            for id in [document_id(url), crate::identity::site_id(url)] {
                assert!(runtime
                    .entities(&token)
                    .unwrap()
                    .get_entity(id)
                    .await
                    .unwrap()
                    .is_none());
            }
        }
        let cached = settle_content(
            &runtime,
            &token,
            &from,
            Some("text/html"),
            200,
            Some("cached"),
            None,
            Some((b"<p>cached</p>".to_vec(), false)),
        )
        .await
        .unwrap();
        let before = runtime
            .entities(&token)
            .unwrap()
            .get_entity(cached.id)
            .await
            .unwrap()
            .unwrap();
        assert!(settle_redirect_hops(&runtime, &token, &[hop])
            .await
            .unwrap()
            .is_empty());
        let after = runtime
            .entities(&token)
            .unwrap()
            .get_entity(cached.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(after).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        assert!(runtime
            .entities(&token)
            .unwrap()
            .get_entity(document_id(&to))
            .await
            .unwrap()
            .is_none());
    }
}

// A temporary-only address stays absent, while a permanent hop owns both
// endpoints. Every hop remains in the receipt, including the skipped ones.
#[tokio::test]
async fn mixed_chain_only_materializes_permanent_endpoints_and_fetched_terminal() {
    let (runtime, token, _dir) = fixture().await;
    let urls: Vec<Url> = [
        "https://a.example/start",
        "https://b.example/old",
        "https://c.example/new",
        "https://d.example/final",
    ]
    .into_iter()
    .map(|value| Url::parse(value).unwrap())
    .collect();
    let hops: Vec<RedirectHop> = [302, 301, 307]
        .into_iter()
        .enumerate()
        .map(|(index, status)| RedirectHop {
            from: urls[index].clone(),
            to: urls[index + 1].clone(),
            status,
        })
        .collect();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("content-type", "text/html".parse().unwrap());
    let reply = settle(
        &runtime,
        &token,
        "GET",
        &urls[3],
        200,
        &headers,
        Some((b"<p>terminal</p>".to_vec(), false)),
        &hops,
        true,
    )
    .await
    .unwrap();
    assert!(runtime
        .entities(&token)
        .unwrap()
        .get_entity(document_id(&urls[0]))
        .await
        .unwrap()
        .is_none());
    assert!(runtime
        .entities(&token)
        .unwrap()
        .get_entity(crate::identity::site_id(&urls[0]))
        .await
        .unwrap()
        .is_none());
    let b = runtime
        .entities(&token)
        .unwrap()
        .get_entity(document_id(&urls[1]))
        .await
        .unwrap()
        .unwrap();
    let c = runtime
        .entities(&token)
        .unwrap()
        .get_entity(document_id(&urls[2]))
        .await
        .unwrap()
        .unwrap();
    let d = runtime
        .entities(&token)
        .unwrap()
        .get_entity(document_id(&urls[3]))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(b.properties.as_ref().unwrap()["status"], 301);
    assert!(c.properties.as_ref().unwrap()["status"].is_null());
    assert!(c.properties.as_ref().unwrap().get("redirect_to").is_none());
    assert_eq!(d.properties.as_ref().unwrap()["status"], 200);
    assert_eq!(d.entity_type.as_deref(), Some("page"));
    assert!(
        reply["body"].is_null(),
        "persisted fetch returns no inline body"
    );
    let supersedes = runtime
        .neighbors(
            &token,
            c.id,
            Direction::Out,
            None,
            Some(vec![EdgeRelation::Supersedes]),
        )
        .await
        .unwrap();
    assert_eq!(supersedes.len(), 1);
    assert_eq!(supersedes[0].node_id, b.id);
    let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
    let receipt = runtime
        .notes(&token)
        .unwrap()
        .get_note(receipt_id)
        .await
        .unwrap()
        .unwrap();
    let request = &receipt.properties.as_ref().unwrap()["request"];
    assert_eq!(request["redirects"], 3);
    assert_eq!(
        request["redirect_chain"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hop| hop["status"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![302, 301, 307]
    );
    let mut annotated: Vec<Uuid> = runtime
        .neighbors(
            &token,
            receipt_id,
            Direction::Out,
            None,
            Some(vec![EdgeRelation::Annotates]),
        )
        .await
        .unwrap()
        .into_iter()
        .map(|hit| hit.node_id)
        .collect();
    annotated.sort();
    let mut expected = vec![b.id, c.id, d.id];
    expected.sort();
    assert_eq!(annotated, expected);
}

// D4/A1.2 require the metadata receipt even when persist=false. HEAD keeps
// this control independent of the separately scoped transient-body changes.
#[tokio::test]
async fn transient_head_still_records_standalone_metadata_receipt() {
    let (runtime, token, _dir) = fixture().await;
    let url = Url::parse("https://metadata.example/head").unwrap();
    let reply = settle(
        &runtime,
        &token,
        "HEAD",
        &url,
        200,
        &reqwest::header::HeaderMap::new(),
        None,
        &[],
        false,
    )
    .await
    .unwrap();
    assert!(reply["id"].is_null());
    let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
    let receipt = runtime
        .notes(&token)
        .unwrap()
        .get_note(receipt_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.kind, "observation");
    let request = &receipt.properties.as_ref().unwrap()["request"];
    assert_eq!(request["method"], "HEAD");
    assert_eq!(request["final_url"], url.as_str());
    assert_eq!(request["bytes"], 0);
    assert!(request["size"].is_null());
    assert!(request["content_digest"].is_null());
    chrono::DateTime::parse_from_rfc3339(request["fetched_at"].as_str().unwrap()).unwrap();
    assert!(reply["body"].is_null());
    assert!(request["content_ref"].is_null());
    assert!(runtime
        .neighbors(
            &token,
            receipt_id,
            Direction::Out,
            None,
            Some(vec![EdgeRelation::Annotates])
        )
        .await
        .unwrap()
        .is_empty());
    assert!(runtime
        .attachments()
        .unwrap()
        .list_attachments(receipt_id)
        .await
        .unwrap()
        .is_empty());
    assert!(runtime
        .entities(&token)
        .unwrap()
        .get_entity(document_id(&url))
        .await
        .unwrap()
        .is_none());
}

// Must fail if persist=false puts a blob, loses binary bytes, or omits receipt metadata.
#[tokio::test]
async fn transient_get_returns_exact_body_and_receipt_without_blob_storage() {
    for configured_store in [false, true] {
        let (runtime, token, _dir) = if configured_store {
            fixture().await
        } else {
            let runtime = KhiveRuntime::memory().unwrap();
            let token = runtime.authorize(Namespace::local()).unwrap();
            (runtime, token, tempfile::tempdir().unwrap())
        };
        for body in [vec![0, 255, 128, 10, 65], vec![]] {
            let url = Url::parse("https://transient.example/final.bin").unwrap();
            let digest = blake3::hash(&body).to_hex().to_string();
            let reference = ContentRef::from_hex(&digest).unwrap();
            let before = chrono::Utc::now();
            let reply = settle(
                &runtime,
                &token,
                "GET",
                &url,
                200,
                &reqwest::header::HeaderMap::new(),
                Some((body.clone(), true)),
                &[],
                false,
            )
            .await
            .expect("transient fetch requires no blob store");
            let after = chrono::Utc::now();
            let encoded = reply["body"]
                .as_str()
                .expect("TRANSIENT_BODY_STANDARD_BASE64_STRING");
            assert_eq!(
                BASE64.decode(encoded).expect("standard padded base64"),
                body,
                "transient response preserves exact binary and empty bytes"
            );
            assert_eq!(
                encoded,
                BASE64.encode(&body),
                "standard alphabet and padding"
            );
            assert_eq!(reply["bytes"], body.len() as u64);
            assert_eq!(reply["truncated"], true);
            assert!(reply["id"].is_null());
            assert!(reply["content_ref"].is_null());
            let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
            let receipt = runtime
                .notes(&token)
                .unwrap()
                .get_note(receipt_id)
                .await
                .unwrap()
                .unwrap();
            let request = &receipt.properties.as_ref().unwrap()["request"];
            assert_eq!(request["final_url"], url.as_str());
            assert_eq!(request["content_digest"], digest);
            assert_eq!(request["size"], body.len() as u64);
            assert!(request["content_ref"].is_null());
            assert!(request.get("body").is_none());
            let fetched_at =
                chrono::DateTime::parse_from_rfc3339(request["fetched_at"].as_str().unwrap())
                    .unwrap();
            assert!(fetched_at >= before && fetched_at <= after);
            for id in [document_id(&url), crate::identity::site_id(&url)] {
                assert!(runtime
                    .entities(&token)
                    .unwrap()
                    .get_entity(id)
                    .await
                    .unwrap()
                    .is_none());
            }
            for id in [document_id(&url), receipt_id] {
                assert!(runtime
                    .attachments()
                    .unwrap()
                    .list_attachments(id)
                    .await
                    .unwrap()
                    .is_empty());
            }
            if let Some(store) = runtime.blob_store() {
                assert!(!store.exists(&reference).await.unwrap());
            }
        }
    }
}

#[tokio::test]
async fn transient_size_refusal_precedes_receipt_and_counts_header_metadata() {
    let (runtime, token, _dir) = fixture().await;
    let url = Url::parse("https://transient.example/oversize.bin").unwrap();
    let mut headers = reqwest::header::HeaderMap::new();
    // JSON doubles these allowed header bytes: the guard must count the
    // serialized value, not only the HTTP header's byte length.
    headers.insert("etag", "\"".repeat(2048).parse().unwrap());
    let error = settle(
        &runtime,
        &token,
        "GET",
        &url,
        200,
        &headers,
        Some((vec![255; INLINE_RAW_BODY_LIMIT as usize], false)),
        &[],
        false,
    )
    .await
    .expect_err("INLINE_METADATA_REFUSED");
    assert!(
        error.to_string().contains("inline response budget"),
        "INLINE_METADATA_REFUSED: {error}"
    );
    assert_no_receipts(&runtime, &token, "INLINE_METADATA_NO_RECEIPT").await;
}

async fn assert_no_receipts(runtime: &KhiveRuntime, token: &NamespaceToken, marker: &str) {
    let notes = runtime
        .notes(token)
        .unwrap()
        .query_notes_filtered(
            token.namespace().as_str(),
            &khive_storage::NoteFilter::default(),
            khive_storage::types::PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert!(notes.items.is_empty(), "{marker}");
}

#[tokio::test]
async fn transient_settlement_rejects_body_above_inline_budget_before_receipt() {
    let (runtime, token, _dir) = fixture().await;
    let url = Url::parse("https://transient.example/body-too-large.bin").unwrap();
    let error = settle(
        &runtime,
        &token,
        "GET",
        &url,
        200,
        &reqwest::header::HeaderMap::new(),
        Some((vec![255; INLINE_RAW_BODY_LIMIT as usize + 1], false)),
        &[],
        false,
    )
    .await
    .expect_err("INLINE_BODY_REFUSED");
    assert!(
        error.to_string().contains("inline response budget"),
        "INLINE_BODY_REFUSED: {error}"
    );
    assert_no_receipts(&runtime, &token, "INLINE_BODY_NO_RECEIPT").await;
}

#[tokio::test]
async fn transient_get_exact_inline_budget_succeeds_with_normal_headers() {
    let (runtime, token, _dir) = fixture().await;
    assert_eq!(INLINE_RAW_BODY_LIMIT, 6_288_384);
    let body = vec![255; INLINE_RAW_BODY_LIMIT as usize];
    let url = Url::parse("https://transient.example/boundary.bin").unwrap();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("content-type", "application/octet-stream".parse().unwrap());
    headers.insert("content-length", body.len().to_string().parse().unwrap());
    headers.insert("etag", "\"boundary\"".parse().unwrap());
    headers.insert(
        "last-modified",
        "Wed, 23 Sep 2026 12:00:00 GMT".parse().unwrap(),
    );
    let reply = settle(
        &runtime,
        &token,
        "GET",
        &url,
        200,
        &headers,
        Some((body.clone(), false)),
        &[],
        false,
    )
    .await
    .expect("INLINE_EXACT_BUDGET_SUCCEEDS");
    let encoded = reply["body"].as_str().expect("INLINE_EXACT_BUDGET_BASE64");
    assert_eq!(
        encoded.len(),
        INLINE_BODY_BUDGET,
        "INLINE_EXACT_BODY_BUDGET"
    );
    assert_eq!(BASE64.decode(encoded).unwrap(), body, "INLINE_EXACT_BYTES");
    let result_len = serde_json::to_vec(&reply).unwrap().len();
    assert!(result_len > INLINE_BODY_BUDGET);
    assert!(
        result_len <= INLINE_RESULT_BUDGET,
        "INLINE_EXACT_RESULT_BUDGET"
    );
    let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
    assert!(runtime
        .notes(&token)
        .unwrap()
        .get_note(receipt_id)
        .await
        .unwrap()
        .is_some());
    assert!(reply["id"].is_null());
    assert!(reply["content_ref"].is_null());
}

#[tokio::test]
async fn transient_get_inline_budget_refusal_precedes_dns_network_and_receipt() {
    use crate::egress::resolver_fixture::ScriptedResolver;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (runtime, token, _dir) = fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = Url::parse(&format!("http://127.0.0.1:{}/budget", address.port())).unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let server_hits = hits.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        server_hits.fetch_add(1, Ordering::SeqCst);
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
    });
    let clients = egress::PinnedClients::default();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut resolver = ScriptedResolver::new(None);
    // Removing the preflight is observable as DNS, then safely refused by
    // the existing loopback policy before any external connection is possible.
    resolver.address = address.ip();
    for caller_bound in [false, true] {
        let mut args = json!({"url": url.as_str(), "persist": false});
        let config = if caller_bound {
            args["max_bytes"] = json!(INLINE_RAW_BODY_LIMIT + 1);
            WebSectionConfig::default()
        } else {
            WebSectionConfig {
                max_bytes_default: Some(INLINE_RAW_BODY_LIMIT + 1),
                ..Default::default()
            }
        };
        let result = run_fetch(
            &runtime,
            &token,
            &resolver,
            &config,
            serde_json::from_value(args).unwrap(),
            &clients,
        )
        .await;
        let error = result.expect_err("INLINE_PREFLIGHT_REFUSED");
        assert!(
            matches!(&error, RuntimeError::InvalidInput(_)),
            "INLINE_PREFLIGHT_REFUSED: expected invalid params: {error}"
        );
        let message = error.to_string();
        assert!(
            message.contains("inline response budget")
                && message.contains("6288384")
                && message.contains("max_bytes")
                && message.contains("persist=true"),
            "INLINE_PREFLIGHT_REFUSED: {error}"
        );
        assert_eq!(
            resolver.calls.load(Ordering::SeqCst),
            0,
            "INLINE_PREFLIGHT_NO_DNS"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "INLINE_PREFLIGHT_NO_NETWORK"
        );
        assert_no_receipts(&runtime, &token, "INLINE_PREFLIGHT_NO_RECEIPT").await;
    }
    // Prove the listener/client witness can observe a real request. This
    // transport-only positive control bypasses the production loopback refusal.
    let outcome = run_one_hop(
        &client,
        &url,
        reqwest::Method::GET,
        &[],
        2,
        Instant::now() + std::time::Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert_eq!(outcome.body.unwrap().0, b"ok");
    tokio::time::timeout(std::time::Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "INLINE_NETWORK_WITNESS_ACTIVE"
    );
}

#[tokio::test]
async fn transient_inline_preflight_admits_boundary_head_and_persisted_fetch() {
    use crate::egress::resolver_fixture::ScriptedResolver;
    use std::sync::atomic::Ordering;

    let (runtime, token, _dir) = fixture().await;
    for (method, persist, max_bytes) in [
        ("GET", false, INLINE_RAW_BODY_LIMIT),
        ("HEAD", false, INLINE_RAW_BODY_LIMIT + 1),
        ("GET", true, INLINE_RAW_BODY_LIMIT + 1),
    ] {
        let mut resolver = ScriptedResolver::new(None);
        resolver.address = "127.0.0.1".parse().unwrap();
        let params = serde_json::from_value(json!({
            "url": "https://inline-boundary.example/", "method": method,
            "persist": persist, "max_bytes": max_bytes,
        }))
        .unwrap();
        let error = run_fetch(
            &runtime,
            &token,
            &resolver,
            &WebSectionConfig::default(),
            params,
            &egress::PinnedClients::default(),
        )
        .await
        .expect_err("fixture DNS is refused as loopback");
        assert!(
            error.to_string().contains("address_loopback"),
            "INLINE_PREFLIGHT_ALLOWED: {error}"
        );
        assert_eq!(
            resolver.calls.load(Ordering::SeqCst),
            1,
            "INLINE_PREFLIGHT_REACHES_DNS"
        );
    }
    assert_no_receipts(&runtime, &token, "INLINE_LOOPBACK_NO_RECEIPT").await;
}
