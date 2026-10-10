use super::*;
use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::engine_config::WebCredentialConfig;

use khive_runtime::{Namespace, VerbRegistryBuilder};
use khive_storage::{
    BlobStore, ContentRef, Direction, EntityFilter, PageRequest, StorageError, StorageResult,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

#[test]
fn allowed_headers_keep_every_link_field_for_later_extraction() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.append("link", "</next>; rel=next".parse().unwrap());
    headers.append("link", "</license>; rel=license".parse().unwrap());
    headers.insert("x-private", "discard".parse().unwrap());
    let kept = extract_allowed_headers(&headers);
    assert_eq!(
        kept["link"],
        json!(["</next>; rel=next", "</license>; rel=license"])
    );
    assert!(kept.get("x-private").is_none());
}

/// The in-crate test runtime carries no `VerbRegistry`, so the web
/// pack's own `EDGE_RULES` (`site contains page|resource`) are never
/// installed on it by default — `link(EdgeRelation::Contains, ...)`
/// then refuses every one of these tests against the base allowlist
/// alone. Register kg+web through a throwaway registry purely to read
/// back their combined `all_edge_rules()` and install it, matching how
/// `khive-pack-kg`/`khive-pack-workspace` tests register their own
/// rules.
fn install_web_edge_rules(runtime: &KhiveRuntime) {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(WebPack::new(runtime.clone()));
    let registry = builder.build().expect("kg+web registry builds");
    runtime.install_edge_rules(registry.all_edge_rules());
}

async fn test_runtime() -> (KhiveRuntime, NamespaceToken, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0)
        .expect("fs blob store");
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    install_web_edge_rules(&runtime);
    runtime
        .install_blob_store(Arc::new(store))
        .expect("install blob store");
    let token = runtime.authorize(Namespace::local()).expect("authorize");
    (runtime, token, dir)
}

#[tokio::test(start_paused = true)]
async fn dns_stall_is_bounded_by_fetch_total_deadline_before_any_body_is_stored() {
    use crate::egress::resolver_fixture::ScriptedResolver;
    let (runtime, token, dir) = test_runtime().await;
    for phase in [1, 2] {
        let resolver = ScriptedResolver::new(Some(phase));
        let params =
            serde_json::from_value(json!({"url":"https://example.test/", "timeout_s":1})).unwrap();
        let start = tokio::time::Instant::now();
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            run_fetch(
                &runtime,
                &token,
                &resolver,
                &WebSectionConfig::default(),
                params,
                &egress::PinnedClients::default(),
            ),
        )
        .await
        .expect("fetch must finish within its one-second bound, not the watchdog")
        .unwrap_err();
        assert!(error.to_string().contains("response_too_slow"), "{error}");
        assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(1));
        assert_eq!(resolver.calls.load(Ordering::SeqCst), phase);
        assert!(resolver.cancelled.load(Ordering::SeqCst));
        assert_eq!(count_blob_files(dir.path()), 0);
    }
    // A prompt answer reaches normal address classification rather than timeout.
    let mut resolver = ScriptedResolver::new(None);
    resolver.address = "127.0.0.1".parse().unwrap();
    let params =
        serde_json::from_value(json!({"url":"https://example.test/", "timeout_s":1})).unwrap();
    let error = run_fetch(
        &runtime,
        &token,
        &resolver,
        &WebSectionConfig::default(),
        params,
        &egress::PinnedClients::default(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("address_loopback"), "{error}");
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn partial_ceiling_config_refuses_programmatic_fetch_before_resolution() {
    use crate::egress::resolver_fixture::ScriptedResolver;
    let (runtime, token, _dir) = test_runtime().await;
    let mut resolver = ScriptedResolver::new(None);
    // The baseline reaches DNS; a loopback answer makes that regression
    // fail without opening an external connection.
    resolver.address = "127.0.0.1".parse().unwrap();
    for config in [
        WebSectionConfig {
            timeout_max_s: Some(1),
            ..Default::default()
        },
        WebSectionConfig {
            max_bytes_max: Some(1),
            ..Default::default()
        },
        WebSectionConfig {
            timeout_max_s: Some(u64::MAX),
            ..Default::default()
        },
    ] {
        let params = serde_json::from_value(json!({"url":"https://example.test/"})).unwrap();
        let error = run_fetch(
            &runtime,
            &token,
            &resolver,
            &config,
            params,
            &egress::PinnedClients::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("invalid_web_config"), "{error}");
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    }
}

/// Counts stored blob objects only — skips the store's own root
/// write-lock file (`.khive-blob-write.lock`, created on the FIRST
/// `put` and left behind for the life of the store; not a blob). A count
/// that includes that lock file reads 2 where it should read 1 after
/// exactly one `put`.
fn count_blob_files(path: &std::path::Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                count += count_blob_files(&p);
            } else if p
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| !n.starts_with('.'))
                .unwrap_or(true)
            {
                count += 1;
            }
        }
    }
    count
}

fn http_response(
    status: u16,
    reason: &str,
    extra_headers: &[(&str, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
    for (name, value) in extra_headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(body);
    out
}

fn http_head_response(
    status: u16,
    reason: &str,
    extra_headers: &[(&str, String)],
    content_length: u64,
) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
    for (name, value) in extra_headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(format!("Content-Length: {content_length}\r\n").as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out
}

async fn wait_for_hits(accepted: &mut watch::Receiver<usize>, expected: usize) {
    tokio::time::timeout(
        Duration::from_secs(10),
        accepted.wait_for(|count| *count >= expected),
    )
    .await
    .expect("accept count acknowledgement exceeds the watchdog")
    .expect("server closed before acknowledging the accept count");
}

async fn spawn_once(response: Vec<u8>) -> (u16, Arc<AtomicUsize>, watch::Receiver<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_task = hits.clone();
    let (accepted_task, accepted) = watch::channel(0);
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let count = hits_task.fetch_add(1, Ordering::SeqCst) + 1;
            accepted_task.send_replace(count);
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
        }
    });
    (port, hits, accepted)
}

/// Serve `responses` in order across sequential connections on ONE
/// listener (one bind, one port): unlike `spawn_once`, a caller can dial
/// the SAME address twice and get two different canned responses — the
/// shape the repeat-fetch test below needs to hit the identical url on
/// both requests.
async fn spawn_sequence(
    responses: Vec<Vec<u8>>,
) -> (u16, Arc<AtomicUsize>, watch::Receiver<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_task = hits.clone();
    let (accepted_task, accepted) = watch::channel(0);
    tokio::spawn(async move {
        for response in responses {
            if let Ok((mut stream, _)) = listener.accept().await {
                let count = hits_task.fetch_add(1, Ordering::SeqCst) + 1;
                accepted_task.send_replace(count);
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(&response).await;
                let _ = stream.shutdown().await;
            }
        }
    });
    (port, hits, accepted)
}

async fn spawn_once_delayed(
    response: Vec<u8>,
    delay: std::time::Duration,
) -> (u16, Arc<AtomicUsize>, watch::Receiver<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_task = hits.clone();
    let (accepted_task, accepted) = watch::channel(0);
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let count = hits_task.fetch_add(1, Ordering::SeqCst) + 1;
            accepted_task.send_replace(count);
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            tokio::time::sleep(delay).await;
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
        }
    });
    (port, hits, accepted)
}

fn plain_client(timeout: std::time::Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .expect("plain client builds")
}

fn local_url(port: u16, path: &str) -> Url {
    Url::parse(&format!("http://127.0.0.1:{port}{path}")).expect("valid local url")
}

async fn entity_count(runtime: &KhiveRuntime, token: &NamespaceToken) -> usize {
    runtime
        .entities(token)
        .expect("entity store capability")
        .query_entities(
            "local",
            EntityFilter::default(),
            PageRequest {
                offset: 0,
                limit: 1_000,
            },
        )
        .await
        .expect("query entities")
        .items
        .len()
}

async fn run_hop_and_settle(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    method: reqwest::Method,
    url: &Url,
    max_bytes: u64,
) -> (HopOutcome, Value) {
    let client = plain_client(Duration::from_secs(5));
    let outcome = run_one_hop(
        &client,
        url,
        method.clone(),
        &[],
        max_bytes,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .expect("hop succeeds");
    let method_name = method.to_string();
    let reply = settle(
        runtime,
        token,
        &method_name,
        &outcome.final_url,
        outcome.status,
        &outcome.headers,
        outcome.body.clone(),
        &[],
        true,
    )
    .await
    .expect("settle");
    (outcome, reply)
}

// A1 (+ former arm 11/16/30): fetch mints site+page/resource+blob+receipt;
// an identical second fetch of the SAME address (same listener, second
// request) returns the same blob reference, mints no new entity, and
// writes a receipt only; a different body is the control that yields a
// different reference.
// A1.2: receipts never own the fetched body. Must fail if receipt rooting
// returns or entity rooting is removed. Control: a fresh HEAD roots nothing.
#[tokio::test]
async fn fetched_body_is_rooted_only_on_entity_head_roots_nothing() {
    let (runtime, token, _dir) = test_runtime().await;
    let url = Url::parse("https://rooted.example.test/page.html").unwrap();
    let body = b"<html><body>rooted body</body></html>".to_vec();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("content-type", "text/html".parse().unwrap());
    let reply = settle(
        &runtime,
        &token,
        "GET",
        &url,
        200,
        &headers,
        Some((body.clone(), false)),
        &[],
        true,
    )
    .await
    .expect("settle persists");
    let entity_id = Uuid::parse_str(reply["id"].as_str().unwrap()).unwrap();
    let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
    let content_ref = reply["content_ref"].as_str().unwrap().to_string();

    let attachments = runtime.attachments().expect("attachment store");
    let on_entity = attachments
        .list_attachments(entity_id)
        .await
        .expect("list entity attachments");
    assert_eq!(on_entity.len(), 1, "one content attachment on the page");
    assert_eq!(on_entity[0].role, "content");
    assert_eq!(on_entity[0].content_ref.to_string(), content_ref);
    assert_eq!(on_entity[0].size_bytes, Some(body.len() as u64));
    let on_receipt = attachments
        .list_attachments(receipt_id)
        .await
        .expect("list receipt attachments");
    assert!(
        on_receipt.is_empty(),
        "the receipt never roots a fetched body"
    );

    let head_url = Url::parse("https://rooted.example.test/other.html").unwrap();
    let head_reply = settle(
        &runtime,
        &token,
        "HEAD",
        &head_url,
        200,
        &headers,
        None,
        &[],
        true,
    )
    .await
    .expect("HEAD settles");
    let head_entity = Uuid::parse_str(head_reply["id"].as_str().unwrap()).unwrap();
    let head_receipt = Uuid::parse_str(head_reply["receipt_id"].as_str().unwrap()).unwrap();
    assert!(attachments
        .list_attachments(head_entity)
        .await
        .unwrap()
        .is_empty());
    assert!(attachments
        .list_attachments(head_receipt)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a1_fetch_mints_entities_repeat_fetch_reuses_blob_writes_receipt_only() {
    let (runtime, token, dir) = test_runtime().await;
    let body = b"<html><body>hi</body></html>".to_vec();
    let response = http_response(
        200,
        "OK",
        &[("Content-Type", "text/html".to_string())],
        &body,
    );
    let (port, hits, mut accepted) = spawn_sequence(vec![response.clone(), response]).await;
    let url = local_url(port, "/page");
    let (_outcome, reply) =
        run_hop_and_settle(&runtime, &token, reqwest::Method::GET, &url, 10_000).await;
    wait_for_hits(&mut accepted, 1).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let id = reply["id"].as_str().expect("entity id").to_string();
    let content_ref = reply["content_ref"].as_str().unwrap().to_string();
    assert_eq!(count_blob_files(dir.path()), 1);

    let entity = runtime
        .entities(&token)
        .unwrap()
        .get_entity(uuid::Uuid::parse_str(&id).unwrap())
        .await
        .unwrap()
        .expect("entity persisted");
    assert_eq!(entity.entity_type.as_deref(), Some("page"));
    let props = entity.properties.unwrap();
    assert_eq!(props["blob_ref"], content_ref);

    // Repeat fetch of the identical url, against the SAME listener/
    // address: same blob ref, no new object, no new entity, still
    // exactly one receipt-bearing note beyond the first.
    let before_notes = runtime
        .list_notes(&token, Some("observation"), 100, 0)
        .await
        .unwrap()
        .len();
    let before_entities = entity_count(&runtime, &token).await;
    let (_outcome2, reply2) =
        run_hop_and_settle(&runtime, &token, reqwest::Method::GET, &url, 10_000).await;
    wait_for_hits(&mut accepted, 2).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "the same address was hit twice"
    );
    assert_eq!(
        reply2["id"], id,
        "the repeat resolves to the same entity, by address"
    );
    assert_eq!(reply2["content_ref"], content_ref);
    assert_eq!(count_blob_files(dir.path()), 1, "no new object stored");
    let after_entities = entity_count(&runtime, &token).await;
    assert_eq!(
        after_entities, before_entities,
        "no new entity minted by the repeat"
    );
    let after_notes = runtime
        .list_notes(&token, Some("observation"), 100, 0)
        .await
        .unwrap()
        .len();
    assert_eq!(after_notes, before_notes + 1, "exactly one new receipt");

    // Control: a different body, at a different address, yields a
    // different reference.
    let (port3, _hits3, _accepted3) =
        spawn_once(http_response(200, "OK", &[], b"different body")).await;
    let url3 = local_url(port3, "/other");
    let (_outcome3, reply3) =
        run_hop_and_settle(&runtime, &token, reqwest::Method::GET, &url3, 10_000).await;
    assert_ne!(reply3["content_ref"], content_ref);
}

// A3: a 301 chain yields `new supersedes old`; a 302 yields no edge and
// a receipt naming the hop. Exercised directly against `settle` with a
// synthetic redirect_hops slice (the transport half of following actual
// redirects is `run_fetch`'s loop, covered structurally by arm21/arm24
// below; this arm is the one that owns the entity/edge assertions).
#[tokio::test]
async fn a3_permanent_redirect_supersedes_temporary_redirect_no_edge() {
    let (runtime, token, _dir) = test_runtime().await;
    let old = Url::parse("https://old.example.test/moved").unwrap();
    let new = Url::parse("https://new.example.test/here").unwrap();
    let hop = RedirectHop {
        from: old.clone(),
        to: new.clone(),
        status: 301,
    };
    let reply = settle(
        &runtime,
        &token,
        "GET",
        &new,
        200,
        &reqwest::header::HeaderMap::new(),
        Some((b"landed".to_vec(), false)),
        &[hop],
        true,
    )
    .await
    .expect("settle");
    let new_id = uuid::Uuid::parse_str(reply["id"].as_str().unwrap()).unwrap();
    let old_id = identity::document_id(
        identity::site_id(
            &khive_types::Namespace::local(),
            &identity::canonicalize(old.clone()),
        ),
        &identity::path_and_query(&identity::canonicalize(old)),
    );
    let neighbors = runtime
        .neighbors(&token, new_id, Direction::Out, None, None)
        .await
        .unwrap();
    assert!(
        neighbors
            .iter()
            .any(|n| n.node_id == old_id && n.relation == EdgeRelation::Supersedes),
        "301 chain must yield new supersedes old"
    );

    // Control: a 302 hop yields no supersedes edge, just a receipt
    // naming the hop.
    let temp_old = Url::parse("https://temp.example.test/a").unwrap();
    let temp_new = Url::parse("https://temp.example.test/b").unwrap();
    let temp_hop = RedirectHop {
        from: temp_old.clone(),
        to: temp_new.clone(),
        status: 302,
    };
    let reply2 = settle(
        &runtime,
        &token,
        "GET",
        &temp_new,
        200,
        &reqwest::header::HeaderMap::new(),
        Some((b"landed2".to_vec(), false)),
        &[temp_hop],
        true,
    )
    .await
    .expect("settle");
    let temp_new_id = uuid::Uuid::parse_str(reply2["id"].as_str().unwrap()).unwrap();
    let temp_old_id = identity::document_id(
        identity::site_id(
            &khive_types::Namespace::local(),
            &identity::canonicalize(temp_old.clone()),
        ),
        &identity::path_and_query(&identity::canonicalize(temp_old)),
    );
    let neighbors2 = runtime
        .neighbors(&token, temp_new_id, Direction::Out, None, None)
        .await
        .unwrap();
    assert!(
        !neighbors2.iter().any(|n| n.node_id == temp_old_id),
        "302 must not yield an edge"
    );
    let receipt_id = uuid::Uuid::parse_str(reply2["receipt_id"].as_str().unwrap()).unwrap();
    let note = runtime
        .notes(&token)
        .unwrap()
        .get_note(receipt_id)
        .await
        .unwrap()
        .unwrap();
    let chain = note.properties.unwrap()["request"]["redirect_chain"].clone();
    assert_eq!(chain[0]["status"], 302, "the hop is named in the receipt");
}

// arm 11: a response larger than the byte bound stores truncated, and
// the stored bytes are exactly the first `max_bytes` bytes of the
// original body. A within-bound response is the positive control.
#[tokio::test]
async fn arm11_over_byte_bound_stores_truncated_prefix() {
    let full_body = vec![b'x'; 100];
    let max_bytes = 40u64;
    let response = http_response(200, "OK", &[], &full_body);
    let (port, hits, mut accepted) = spawn_once(response).await;
    let url = local_url(port, "/big");
    let client = plain_client(Duration::from_secs(5));
    let outcome = run_one_hop(
        &client,
        &url,
        reqwest::Method::GET,
        &[],
        max_bytes,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .expect("hop succeeds");
    wait_for_hits(&mut accepted, 1).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.status, 200);
    let (buffer, truncated) = outcome.body.clone().expect("GET carries a body slot");
    assert!(truncated);
    assert_eq!(buffer.len() as u64, max_bytes);
    assert_eq!(buffer, full_body[..max_bytes as usize]);

    let (runtime, token, _dir) = test_runtime().await;
    let reply = settle(
        &runtime,
        &token,
        "GET",
        &outcome.final_url,
        outcome.status,
        &outcome.headers,
        outcome.body.clone(),
        &[],
        true,
    )
    .await
    .expect("settle stores + records receipt");
    assert_eq!(reply["truncated"], true);
    assert_eq!(reply["bytes"], max_bytes);
    let persisted_id = Uuid::parse_str(reply["id"].as_str().unwrap()).unwrap();
    let persisted = runtime
        .entities(&token)
        .unwrap()
        .get_entity(persisted_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.properties.unwrap()["truncated"], true);
    let content_ref = reply["content_ref"]
        .as_str()
        .expect("content_ref")
        .to_string();

    // Positive control: within-bound is not truncated.
    let small_body = vec![b'y'; 10];
    let (port2, _hits2, _accepted2) = spawn_once(http_response(200, "OK", &[], &small_body)).await;
    let url2 = local_url(port2, "/small");
    let outcome2 = run_one_hop(
        &client,
        &url2,
        reqwest::Method::GET,
        &[],
        max_bytes,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .expect("hop succeeds");
    let (buffer2, truncated2) = outcome2.body.expect("GET carries a body slot");
    assert!(!truncated2);
    assert_eq!(buffer2, small_body);

    let store = runtime.require_blob_store().unwrap();
    let content_ref_parsed = ContentRef::from_hex(&content_ref).expect("valid content ref hex");
    let stored = store
        .get_bounded_verified(&content_ref_parsed, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await
        .expect("stored bytes read back and digest-verified");
    assert_eq!(stored, buffer);
}

// arm 12: a response slower than the time bound refuses, and the blob
// store holds nothing new; a within-bound response is the control.
#[tokio::test]
async fn arm12_over_time_bound_refuses_blob_store_holds_nothing_within_bound_succeeds() {
    let (_runtime, _token, dir) = test_runtime().await;
    assert_eq!(count_blob_files(dir.path()), 0, "blob dir starts empty");

    let body = b"too slow".to_vec();
    let (port, hits, mut accepted) = spawn_once_delayed(
        http_response(200, "OK", &[], &body),
        Duration::from_millis(300),
    )
    .await;
    let url = local_url(port, "/slow");
    let client = plain_client(Duration::from_secs(10));
    let err = run_one_hop(
        &client,
        &url,
        reqwest::Method::GET,
        &[],
        1_000,
        Instant::now() + Duration::from_millis(50),
    )
    .await
    .expect_err("a hop past the deadline refuses");
    let message = err.to_string();
    assert!(message.contains("response_too_slow"), "{message}");
    wait_for_hits(&mut accepted, 1).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1, "the connection was made");
    assert_eq!(
        count_blob_files(dir.path()),
        0,
        "no object stored on a timed-out hop"
    );

    let (port2, _hits2, _accepted2) = spawn_once(http_response(200, "OK", &[], b"fast")).await;
    let url2 = local_url(port2, "/fast");
    let outcome = run_one_hop(
        &client,
        &url2,
        reqwest::Method::GET,
        &[],
        1_000,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .expect("within-bound hop succeeds");
    assert_eq!(outcome.status, 200);
}

// A4: refresh's unchanged-etag no-op is exercised in refresh.rs; this is
// the fetch-side half: a HEAD response advertising a nonzero
// content-length yields no content_ref/blob write, and the receipt
// headers match the reply's exactly with an unlisted header dropped
// from both (former arm 28). A GET control reads and stores.
#[tokio::test]
async fn a4_head_response_yields_no_content_ref_get_control_stores() {
    let (runtime, token, dir) = test_runtime().await;
    assert_eq!(count_blob_files(dir.path()), 0);

    let head_headers = [
        ("Content-Type", "text/plain".to_string()),
        ("X-Unlisted", "should-not-appear".to_string()),
    ];
    let (port, hits, mut accepted) =
        spawn_once(http_head_response(200, "OK", &head_headers, 42)).await;
    let url = local_url(port, "/head");
    let client = plain_client(Duration::from_secs(5));
    let outcome = run_one_hop(
        &client,
        &url,
        reqwest::Method::HEAD,
        &[],
        1_000,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .expect("HEAD hop succeeds");
    wait_for_hits(&mut accepted, 1).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(outcome.body.is_none(), "HEAD never carries a body slot");

    let reply = settle(
        &runtime,
        &token,
        "HEAD",
        &outcome.final_url,
        outcome.status,
        &outcome.headers,
        outcome.body.clone(),
        &[],
        true,
    )
    .await
    .expect("settle");
    assert_eq!(reply["content_ref"], Value::Null);
    assert_eq!(reply["bytes"], 0);
    assert_eq!(reply["truncated"], false);
    assert_eq!(
        count_blob_files(dir.path()),
        0,
        "no blob put for a HEAD response"
    );

    let headers = reply["headers"].as_object().unwrap();
    assert_eq!(headers.get("content-type").unwrap(), "text/plain");
    assert!(
        !headers.contains_key("x-unlisted"),
        "unlisted header omitted from the reply"
    );

    let receipt_id = uuid::Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
    let note = runtime
        .notes(&token)
        .unwrap()
        .get_note(receipt_id)
        .await
        .unwrap()
        .unwrap();
    let properties = note.properties.unwrap();
    let record = properties.get("request").unwrap();
    assert_eq!(
        record["headers"], reply["headers"],
        "receipt headers match the reply exactly"
    );

    // GET control: reads and stores its body.
    let get_body = b"actual bytes".to_vec();
    let (port2, _hits2, _accepted2) = spawn_once(http_response(200, "OK", &[], &get_body)).await;
    let url2 = local_url(port2, "/get");
    let outcome2 = run_one_hop(
        &client,
        &url2,
        reqwest::Method::GET,
        &[],
        1_000,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .expect("GET hop succeeds");
    let reply2 = settle(
        &runtime,
        &token,
        "GET",
        &outcome2.final_url,
        outcome2.status,
        &outcome2.headers,
        outcome2.body.clone(),
        &[],
        true,
    )
    .await
    .expect("settle");
    assert!(reply2["content_ref"].is_string());
    assert_eq!(reply2["bytes"], get_body.len() as u64);
    assert_eq!(
        count_blob_files(dir.path()),
        1,
        "the GET control stores exactly one new object"
    );
}

#[tokio::test]
async fn head_receipt_and_row_leave_unread_size_unknown_get_transient_records_measured_size() {
    let (runtime, token, _dir) = test_runtime().await;
    let url = Url::parse("https://example.test/head-receipt").unwrap();
    let mut head_headers = reqwest::header::HeaderMap::new();
    head_headers.insert("content-length", "42".parse().unwrap());
    let head = settle(
        &runtime,
        &token,
        "HEAD",
        &url,
        200,
        &head_headers,
        None,
        &[],
        true,
    )
    .await
    .unwrap();
    assert_eq!(head["bytes"], 0);
    assert!(head["content_ref"].is_null());
    let head_id = Uuid::parse_str(head["id"].as_str().unwrap()).unwrap();
    let head_row = runtime
        .entities(&token)
        .unwrap()
        .get_entity(head_id)
        .await
        .unwrap()
        .unwrap();
    let head_properties = head_row.properties.unwrap();
    assert!(head_properties["content_digest"].is_null());
    assert!(
        head_properties["size"].is_null(),
        "HEAD read no body length"
    );

    let head_receipt_id = Uuid::parse_str(head["receipt_id"].as_str().unwrap()).unwrap();
    let head_receipt = runtime
        .notes(&token)
        .unwrap()
        .get_note(head_receipt_id)
        .await
        .unwrap()
        .unwrap();
    let head_request = &head_receipt.properties.as_ref().unwrap()["request"];
    assert_eq!(head_request["headers"]["content-length"], "42");
    assert_eq!(head_request["bytes"], 0);
    assert!(head_request.get("content_digest").is_none());
    assert!(head_request.get("size").is_none());

    // An advertised length is not a measured size. A GET with
    // persist=false reads bytes, stores no entity change, and records the
    // body digest and the actual length in its standalone receipt.
    let body = b"read bytes".to_vec();
    let mut get_headers = reqwest::header::HeaderMap::new();
    get_headers.insert("content-length", "999".parse().unwrap());
    let get = settle(
        &runtime,
        &token,
        "GET",
        &url,
        200,
        &get_headers,
        Some((body.clone(), false)),
        &[],
        false,
    )
    .await
    .unwrap();
    assert!(get["id"].is_null());
    let get_receipt_id = Uuid::parse_str(get["receipt_id"].as_str().unwrap()).unwrap();
    let get_receipt = runtime
        .notes(&token)
        .unwrap()
        .get_note(get_receipt_id)
        .await
        .unwrap()
        .unwrap();
    let get_request = &get_receipt.properties.as_ref().unwrap()["request"];
    assert_eq!(get_request["headers"]["content-length"], "999");
    assert_eq!(get_request["bytes"], body.len() as u64);
    assert_eq!(get_request["size"], body.len() as u64);
    assert_eq!(
        get_request["content_digest"],
        blake3::hash(&body).to_hex().to_string()
    );
    assert!(get["content_ref"].is_null());
    assert!(get_request["content_ref"].is_null());
    let after = runtime
        .entities(&token)
        .unwrap()
        .get_entity(head_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.properties.unwrap(), head_properties);
}

// arm 19: a gzip response is refused by name; the compressed bytes never
// reach the caller, whatever the byte bound.
#[tokio::test]
async fn arm19_gzip_response_is_refused_not_passed_through() {
    let plaintext = vec![b'z'; 10_000];
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        use std::io::Write as _;
        encoder.write_all(&plaintext).unwrap();
    }
    let compressed = encoder.finish().unwrap();
    assert!(
        compressed.len() < plaintext.len(),
        "fixture must actually compress"
    );

    let response = http_response(
        200,
        "OK",
        &[("Content-Encoding", "gzip".to_string())],
        &compressed,
    );
    let (port, _hits, _accepted) = spawn_once(response).await;
    let url = local_url(port, "/gz");
    let client = plain_client(Duration::from_secs(5));
    let error = run_one_hop(
        &client,
        &url,
        reqwest::Method::GET,
        &[],
        100,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .expect_err("a content-encoded response must be refused");
    assert!(
        error.to_string().contains("unsupported_content_encoding"),
        "{error}"
    );
}

// arm 21: a redirect whose second hop is outside the credential's host
// set refuses at that hop — hop 1 is made, hop 2 is not.
#[tokio::test]
async fn arm21_redirect_second_hop_outside_credential_set_refuses_hop2_never_dialed() {
    let (hop1_port, hop1_hits, mut hop1_accepted) = spawn_once(http_response(
        302,
        "Found",
        &[("Location", "https://elsewhere.test/next".to_string())],
        b"",
    ))
    .await;
    let (_hop2_port, hop2_hits, _hop2_accepted) =
        spawn_once(http_response(200, "OK", &[], b"never reached")).await;

    let url = local_url(hop1_port, "/start");
    let client = plain_client(Duration::from_secs(5));
    let outcome = run_one_hop(
        &client,
        &url,
        reqwest::Method::GET,
        &[],
        1_000,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .expect("hop 1 executes");
    wait_for_hits(&mut hop1_accepted, 1).await;
    assert_eq!(hop1_hits.load(Ordering::SeqCst), 1, "hop 1 was made");
    assert_eq!(outcome.status, 302);
    let redirect_to = outcome
        .redirect_to
        .expect("a Location header yields a redirect target");
    assert_eq!(redirect_to.host_str(), Some("elsewhere.test"));

    let mut cfg = WebSectionConfig::default();
    cfg.credentials.push(WebCredentialConfig {
        name: "token".to_string(),
        env_var: "UNUSED_TEST_VAR".to_string(),
        hosts: vec!["127.0.0.1".to_string()],
    });
    let err = egress::check_credential(&cfg, "token", redirect_to.host_str().unwrap()).unwrap_err();
    assert_eq!(err.code, "credential_host_mismatch");
    // The refusal never requests hop 2, so there is no accept to acknowledge.
    assert_eq!(hop2_hits.load(Ordering::SeqCst), 0, "hop 2 was not made");

    let mut cfg2 = WebSectionConfig::default();
    cfg2.credentials.push(WebCredentialConfig {
        name: "token".to_string(),
        env_var: "UNUSED_TEST_VAR".to_string(),
        hosts: vec!["elsewhere.test".to_string()],
    });
    assert!(egress::check_credential(&cfg2, "token", redirect_to.host_str().unwrap()).is_ok());
}

// arm 24 (redirect half): a redirect to a URL carrying userinfo refuses
// before the next hop is ever requested.
#[tokio::test]
async fn arm24_redirect_to_userinfo_url_refuses_before_next_hop_is_dialed() {
    let (hop1_port, hop1_hits, mut hop1_accepted) = spawn_once(http_response(
        302,
        "Found",
        &[("Location", "https://user:pass@elsewhere.test/x".to_string())],
        b"",
    ))
    .await;
    let (_hop2_port, hop2_hits, _hop2_accepted) =
        spawn_once(http_response(200, "OK", &[], b"never reached")).await;

    let url = local_url(hop1_port, "/start");
    let client = plain_client(Duration::from_secs(5));
    let outcome = run_one_hop(
        &client,
        &url,
        reqwest::Method::GET,
        &[],
        1_000,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .expect("hop 1 executes");
    wait_for_hits(&mut hop1_accepted, 1).await;
    assert_eq!(hop1_hits.load(Ordering::SeqCst), 1);
    let redirect_to = outcome
        .redirect_to
        .expect("a Location header yields a redirect target");
    assert_eq!(redirect_to.username(), "user");

    let err = egress::check_scheme_and_userinfo(&redirect_to).unwrap_err();
    assert_eq!(err.code, "userinfo_present");
    // The refusal never requests hop 2, so there is no accept to acknowledge.
    assert_eq!(
        hop2_hits.load(Ordering::SeqCst),
        0,
        "the second hop is never requested"
    );

    let mut stripped = redirect_to.clone();
    stripped.set_username("").unwrap();
    stripped.set_password(None).unwrap();
    assert!(egress::check_scheme_and_userinfo(&stripped).is_ok());
}

// arm 25 (dispatch half): max_bytes/timeout_s ceilings refuse when
// wired through the real `handle_fetch` dispatch path.
#[tokio::test]
async fn arm25_dispatch_ceiling_refusals_wired_through_handle_fetch() {
    let (runtime, token, _dir) = test_runtime().await;
    let pack = WebPack::new(runtime.clone());

    let over_bytes = pack
        .handle_fetch(
            &token,
            json!({
                "url": "https://example.test/",
                "max_bytes": khive_runtime::engine_config::WebCeilings::default().max_bytes_max + 1,
            }),
        )
        .await
        .unwrap_err();
    assert!(
        over_bytes.to_string().contains("ceiling_exceeded"),
        "{over_bytes}"
    );

    let over_timeout = pack
        .handle_fetch(
            &token,
            json!({
                "url": "https://example.test/",
                "timeout_s": khive_runtime::engine_config::WebCeilings::default().timeout_max_s + 1,
            }),
        )
        .await
        .unwrap_err();
    assert!(
        over_timeout.to_string().contains("ceiling_exceeded"),
        "{over_timeout}"
    );

    let at_ceiling = pack
        .handle_fetch(
            &token,
            json!({
                "url": "ftp://example.test/",
                "max_bytes": khive_runtime::engine_config::WebCeilings::default().max_bytes_max,
                "timeout_s": khive_runtime::engine_config::WebCeilings::default().timeout_max_s,
            }),
        )
        .await
        .unwrap_err();
    assert!(
        !at_ceiling.to_string().contains("ceiling_exceeded"),
        "{at_ceiling}"
    );
    assert!(
        at_ceiling.to_string().contains("scheme_not_allowed"),
        "{at_ceiling}"
    );
}

#[derive(Debug)]
struct FailingPutBlobStore {
    put_calls: AtomicUsize,
}

impl FailingPutBlobStore {
    fn new() -> Self {
        Self {
            put_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl BlobStore for FailingPutBlobStore {
    async fn put(&self, _bytes: Vec<u8>) -> StorageResult<ContentRef> {
        self.put_calls.fetch_add(1, Ordering::SeqCst);
        Err(StorageError::Transaction {
            operation: "put".into(),
            message: "simulated put failure".to_string(),
        })
    }
    async fn get_bounded_verified(
        &self,
        _content_ref: &ContentRef,
        _max_bytes: u64,
    ) -> StorageResult<Vec<u8>> {
        Err(StorageError::Transaction {
            operation: "get_bounded_verified".into(),
            message: "not used by this test double".to_string(),
        })
    }
    async fn exists(&self, _content_ref: &ContentRef) -> StorageResult<bool> {
        Ok(false)
    }
    async fn size(&self, _content_ref: &ContentRef) -> StorageResult<Option<u64>> {
        Ok(None)
    }
    async fn delete(&self, _content_ref: &ContentRef) -> StorageResult<bool> {
        Err(StorageError::Transaction {
            operation: "delete".into(),
            message: "not used by this test double".to_string(),
        })
    }
}

// arm 29: a put failure prevents the receipt from ever being written,
// and the transport hop is attempted exactly once (no retry) — but the
// entity rows (minted before the blob put) persist regardless, since
// identity is by address and independent of any one fetch's success.
#[tokio::test]
async fn arm29_put_failure_prevents_receipt_entities_still_minted_no_retry() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    install_web_edge_rules(&runtime);
    let failing_store = Arc::new(FailingPutBlobStore::new());
    runtime
        .install_blob_store(failing_store.clone() as Arc<dyn BlobStore>)
        .expect("install failing store");
    let token = runtime.authorize(Namespace::local()).expect("authorize");
    let before = runtime
        .list_notes(&token, Some("observation"), 100, 0)
        .await
        .unwrap()
        .len();

    let body = b"never stored".to_vec();
    let (port, hits, mut accepted) = spawn_once(http_response(200, "OK", &[], &body)).await;
    let url = local_url(port, "/fail");
    let client = plain_client(Duration::from_secs(5));
    let outcome = run_one_hop(
        &client,
        &url,
        reqwest::Method::GET,
        &[],
        1_000,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .expect("the transport hop itself succeeds");
    wait_for_hits(&mut accepted, 1).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let err = settle(
        &runtime,
        &token,
        "GET",
        &outcome.final_url,
        outcome.status,
        &outcome.headers,
        outcome.body.clone(),
        &[],
        true,
    )
    .await
    .expect_err("a failing store refuses settle before any receipt write");
    let message = err.to_string();
    assert!(
        message.contains("simulated put failure") || message.contains("storage"),
        "{message}"
    );
    assert_eq!(
        failing_store.put_calls.load(Ordering::SeqCst),
        1,
        "put attempted exactly once"
    );
    wait_for_hits(&mut accepted, 1).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "no retry of the transport hop"
    );
    let after = runtime
        .list_notes(&token, Some("observation"), 100, 0)
        .await
        .unwrap()
        .len();
    assert_eq!(after, before, "a failed put left no receipt behind");

    let canonical = identity::canonicalize(outcome.final_url.clone());
    let site = identity::site_id(&khive_types::Namespace::local(), &canonical);
    let id = identity::document_id(site, &identity::path_and_query(&canonical));
    assert!(
        runtime
            .entities(&token)
            .unwrap()
            .get_entity(id)
            .await
            .unwrap()
            .is_some(),
        "the entity row was minted before the failing put"
    );
}

// arm 30: a fetched object's put goes through the same content-addressed
// BlobStore contract an ordinary blob.put would use — a local read by
// content_ref succeeds independent of receipt outcome.
#[tokio::test]
async fn arm30_fetch_put_matches_direct_put_content_addressing() {
    let (runtime, token, _dir) = test_runtime().await;
    let store = runtime.require_blob_store().unwrap();

    let payload = b"identical bytes via either path".to_vec();
    let direct_ref = store.put(payload.clone()).await.expect("direct put");

    let (port, _hits, _accepted) = spawn_once(http_response(200, "OK", &[], &payload)).await;
    let url = local_url(port, "/same-bytes");
    let (_outcome, reply) =
        run_hop_and_settle(&runtime, &token, reqwest::Method::GET, &url, 10_000).await;
    let via_fetch_ref = reply["content_ref"].as_str().unwrap();
    assert_eq!(
        via_fetch_ref,
        direct_ref.to_string(),
        "identical bytes content-address identically through either path (idempotent put, ADR-111)"
    );

    let read_back = store
        .get_bounded_verified(&direct_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await
        .expect("read by content_ref alone, independent of receipt outcome");
    assert_eq!(read_back, payload);
}

#[test]
fn classify_entity_type_html_variants_are_page_everything_else_is_resource() {
    assert_eq!(classify_entity_type(Some("text/html")), "page");
    assert_eq!(
        classify_entity_type(Some("text/html; charset=utf-8")),
        "page"
    );
    assert_eq!(classify_entity_type(Some("APPLICATION/XHTML+XML")), "page");
    assert_eq!(classify_entity_type(Some("application/json")), "resource");
    assert_eq!(classify_entity_type(Some("text/plain")), "resource");
    assert_eq!(classify_entity_type(None), "resource");
}

// `accept` becomes the Accept request header, through the exact same
// allow-list `headers` goes through; an explicit headers["Accept"]
// alongside it refuses (naming the conflict) rather than one silently
// winning. Control: accept alone, beside an unrelated allowed header,
// succeeds and both are present.
#[test]
fn accept_param_becomes_accept_header_conflicting_with_headers_refuses() {
    let empty = BTreeMap::new();
    let out = effective_request_headers(&empty, Some("application/json")).unwrap();
    assert_eq!(
        out,
        vec![("Accept".to_string(), "application/json".to_string())]
    );

    let mut conflicting = BTreeMap::new();
    conflicting.insert("Accept".to_string(), "text/plain".to_string());
    let err = effective_request_headers(&conflicting, Some("application/json")).unwrap_err();
    assert_eq!(err.code, "header_conflict");

    let mut with_other = BTreeMap::new();
    with_other.insert("User-Agent".to_string(), "khive".to_string());
    let ok = effective_request_headers(&with_other, Some("text/html")).unwrap();
    assert_eq!(ok.len(), 2);
    assert!(ok.contains(&("User-Agent".to_string(), "khive".to_string())));
    assert!(ok.contains(&("Accept".to_string(), "text/html".to_string())));
}

// The header `effective_request_headers` computes for `accept` actually
// reaches the wire — read back from the raw bytes a real local listener
// received, via `run_one_hop` directly (same unguarded pattern every
// other mechanics test in this module uses).
#[tokio::test]
async fn accept_param_header_reaches_the_wire() {
    let hop_headers =
        effective_request_headers(&BTreeMap::new(), Some("application/vnd.khive+json")).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let received: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let received_task = received.clone();
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            if let Ok(n) = stream.read(&mut buf).await {
                received_task.lock().unwrap().extend_from_slice(&buf[..n]);
            }
            let _ = stream.write_all(&http_response(200, "OK", &[], b"")).await;
            let _ = stream.shutdown().await;
        }
    });

    let client = plain_client(Duration::from_secs(5));
    let url = local_url(port, "/x");
    run_one_hop(
        &client,
        &url,
        reqwest::Method::GET,
        &hop_headers,
        1_000,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .expect("hop succeeds");

    let raw = received.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
    assert!(
        text.contains("accept: application/vnd.khive+json"),
        "{text}"
    );
}

#[tokio::test]
async fn plain_and_pinned_clients_send_fixed_accept_encoding() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let captured = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..6 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut buffer = [0; 1024];
                let count = stream.read(&mut buffer).await.expect("read request");
                assert!(count > 0 && bytes.len() < 16_384);
                bytes.extend_from_slice(&buffer[..count]);
            }
            requests.push(String::from_utf8(bytes).expect("ASCII request headers"));
            stream
                .write_all(&http_response(200, "OK", &[], b""))
                .await
                .expect("respond");
            stream.shutdown().await.expect("shutdown");
        }
        requests
    });
    let plain = plain_client(Duration::from_secs(5));
    let pinned = egress::pinned_client(
        "encoding.example",
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        port,
    )
    .expect("pinned client builds");
    for (client, url, method) in [
        (&plain, local_url(port, "/plain"), reqwest::Method::GET),
        (
            &pinned,
            Url::parse(&format!("http://encoding.example:{port}/pinned-get")).unwrap(),
            reqwest::Method::GET,
        ),
        (
            &pinned,
            Url::parse(&format!("http://encoding.example:{port}/pinned-head")).unwrap(),
            reqwest::Method::HEAD,
        ),
    ] {
        // The built client alone offers no content coding ...
        client
            .request(method.clone(), url.clone())
            .send()
            .await
            .expect("built client sends request");
        // ... and the hop sends the fixed identity value, once.
        run_one_hop(
            client,
            &url,
            method,
            &[],
            1_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("hop succeeds");
    }
    for (index, request) in captured
        .await
        .expect("captured requests")
        .iter()
        .enumerate()
    {
        let offered: Vec<&str> = request
            .lines()
            .filter(|line| line.to_ascii_lowercase().starts_with("accept-encoding:"))
            .map(|line| line.split_once(':').unwrap().1.trim())
            .collect();
        let expected: &[&str] = if index % 2 == 0 { &[] } else { &["identity"] };
        assert_eq!(
            offered, expected,
            "request {index} must offer no compression: {request}"
        );
    }
}

// The redirect hop cap refuses once already at the cap (the
// `max_redirects`+1-th redirect), naming the cap in the message; every
// count below the cap is allowed — the boundary is the whole guard, so
// it is checked at every step from 0 up to and including the cap.
#[test]
fn redirect_cap_refuses_at_cap_allows_below() {
    for redirects in 0..MAX_REDIRECTS {
        assert!(
            egress::check_redirect_cap(redirects, MAX_REDIRECTS).is_ok(),
            "redirects={redirects} must still be allowed"
        );
    }
    let err = egress::check_redirect_cap(MAX_REDIRECTS, MAX_REDIRECTS).unwrap_err();
    assert_eq!(err.code, "redirect_limit_exceeded");
    assert!(
        err.message.contains(&MAX_REDIRECTS.to_string()),
        "{}",
        err.message
    );
}

// Only GET and HEAD are permitted methods. The check runs before
// any address is even parsed, so a refusal needs no network; GET/HEAD
// pass the method gate specifically and proceed to fail for the
// unrelated, deterministic reason that a loopback address always
// refuses (proof that method_not_allowed did NOT fire for them).
#[tokio::test]
async fn post_refuses_get_and_head_pass_the_method_check() {
    let (runtime, token, _dir) = test_runtime().await;
    let pack = WebPack::new(runtime.clone());

    let post = pack
        .handle_fetch(
            &token,
            json!({"url": "http://127.0.0.1:9/x", "method": "POST"}),
        )
        .await
        .unwrap_err();
    assert!(post.to_string().contains("method_not_allowed"), "{post}");

    for method in ["GET", "HEAD"] {
        let err = pack
            .handle_fetch(
                &token,
                json!({"url": "http://127.0.0.1:9/x", "method": method}),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("method_not_allowed"), "{method}: {msg}");
        assert!(
            msg.contains("address_loopback"),
            "{method}: expected the method check to pass and the address check to be \
                 the one that refused: {msg}"
        );
    }
}

// Only the allow-listed response headers are ever surfaced — a
// disallowed header is dropped even though it was actually present in
// the response, both from the reply AND from what the receipt stores;
// an allow-listed header alongside it is kept in both places.
#[tokio::test]
async fn disallowed_response_header_not_persisted_allowed_header_is() {
    let (runtime, token, _dir) = test_runtime().await;
    let body = b"hi".to_vec();
    let response = http_response(
        200,
        "OK",
        &[
            ("Content-Type", "text/plain".to_string()),
            ("X-Powered-By", "leaked".to_string()),
        ],
        &body,
    );
    let (port, _hits, _accepted) = spawn_once(response).await;
    let url = local_url(port, "/x");
    let (_outcome, reply) =
        run_hop_and_settle(&runtime, &token, reqwest::Method::GET, &url, 10_000).await;

    let headers = reply["headers"].as_object().unwrap();
    assert_eq!(headers.get("content-type").unwrap(), "text/plain");
    assert!(
        !headers.contains_key("x-powered-by"),
        "disallowed response header must not reach the reply"
    );

    let receipt_id = uuid::Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
    let note = runtime
        .notes(&token)
        .unwrap()
        .get_note(receipt_id)
        .await
        .unwrap()
        .unwrap();
    let properties = note.properties.unwrap();
    let request = properties.get("request").unwrap();
    let stored_headers = request["headers"].as_object().unwrap();
    assert_eq!(stored_headers.get("content-type").unwrap(), "text/plain");
    assert!(
        !stored_headers.contains_key("x-powered-by"),
        "disallowed response header must not reach the receipt either"
    );
}
#[tokio::test]
async fn head_preserves_a_previously_fetched_body_and_its_attachment() {
    let (runtime, token, dir) = test_runtime().await;
    let url = Url::parse("https://head.example.test/page").unwrap();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("content-type", "text/html".parse().unwrap());
    headers.insert("etag", "\"body-v1\"".parse().unwrap());
    let fetched = settle(
        &runtime,
        &token,
        "GET",
        &url,
        200,
        &headers,
        Some((b"<p>stored body</p>".to_vec(), false)),
        &[],
        true,
    )
    .await
    .unwrap();
    let id = Uuid::parse_str(fetched["id"].as_str().unwrap()).unwrap();
    let before = runtime
        .entities(&token)
        .unwrap()
        .get_entity(id)
        .await
        .unwrap()
        .unwrap();
    let attachments = runtime
        .attachments()
        .unwrap()
        .list_attachments(id)
        .await
        .unwrap();
    let blobs_before = count_blob_files(dir.path());

    let mut head_headers = reqwest::header::HeaderMap::new();
    head_headers.insert("content-type", "application/octet-stream".parse().unwrap());
    head_headers.insert("etag", "\"remote-v2\"".parse().unwrap());
    let head = settle(
        &runtime,
        &token,
        "HEAD",
        &url,
        204,
        &head_headers,
        None,
        &[],
        true,
    )
    .await
    .unwrap();
    assert_eq!(head["id"], fetched["id"]);
    assert!(head["content_ref"].is_null());
    assert_eq!(head["bytes"], 0);
    let after = runtime
        .entities(&token)
        .unwrap()
        .get_entity(id)
        .await
        .unwrap()
        .unwrap();
    let mut expected_properties = before.properties.unwrap();
    expected_properties["status"] = json!(204);
    assert_eq!(after.properties.as_ref().unwrap(), &expected_properties);
    assert_eq!(after.entity_type, before.entity_type);
    let after_attachments = runtime
        .attachments()
        .unwrap()
        .list_attachments(id)
        .await
        .unwrap();
    assert_eq!(after_attachments.len(), attachments.len());
    assert_eq!(after_attachments[0].content_ref, attachments[0].content_ref);
    assert_eq!(after_attachments[0].size_bytes, attachments[0].size_bytes);
    assert_eq!(count_blob_files(dir.path()), blobs_before);
    let head_receipt = Uuid::parse_str(head["receipt_id"].as_str().unwrap()).unwrap();
    assert!(runtime
        .attachments()
        .unwrap()
        .list_attachments(head_receipt)
        .await
        .unwrap()
        .is_empty());

    let empty_get = settle(
        &runtime,
        &token,
        "GET",
        &url,
        200,
        &head_headers,
        Some((Vec::new(), false)),
        &[],
        true,
    )
    .await
    .unwrap();
    assert_ne!(empty_get["content_ref"], fetched["content_ref"]);
    let emptied = runtime
        .entities(&token)
        .unwrap()
        .get_entity(id)
        .await
        .unwrap()
        .unwrap();
    let properties = emptied.properties.unwrap();
    assert_eq!(properties["size"], 0);
    assert_eq!(properties["blob_ref"], empty_get["content_ref"]);
    assert_eq!(emptied.entity_type.as_deref(), Some("resource"));
}
