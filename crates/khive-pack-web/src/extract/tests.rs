use super::*;
use khive_pack_kg::KgPack;
use khive_runtime::VerbRegistryBuilder;
use khive_types::Namespace;
use std::sync::Arc;

mod hydration;

#[test]
fn lossy_decode_reserves_replacement_bytes_without_capacity_growth() {
    for raw in [
        b"abc".as_slice(),
        b"\xff\xff\xff",
        b"a\xf0\x90\x80z\xc2",
        b"",
    ] {
        let required = decoded_body_bytes(raw);
        let decoded = decode_body(raw, required);
        assert_eq!(decoded, String::from_utf8_lossy(raw));
        assert_eq!(decoded.len(), required);
        assert_eq!(decoded.capacity(), required);
        assert!(required <= 3 * raw.len());
    }
    // Must fail if admission counts raw bytes instead of replacement bytes.
    assert_eq!(decoded_body_bytes(b"\xff\xff\xff"), 9);
}

#[test]
fn bounded_text_scanner_preserves_tag_and_whitespace_behavior() {
    let scripts =
        Regex::new(r"(?is)<script\b[^>]*>.*?</script>|<style\b[^>]*>.*?</style>").unwrap();
    let tags = Regex::new(r"(?s)<[^>]+>").unwrap();
    let whitespace = Regex::new(r"\s+").unwrap();
    for body in [
        "  Hello\u{2003} world <br> next  ",
        "before<script>secret <b>text</b></script><style>hidden</style>after",
        "<p title='<script>secret</script>'>visible</p>",
        "literal <> <<> end <unfinished\n  tag",
        "<script>unterminated script",
        "<sCrIpT data='>'>ignored</ScRiPt>after",
        "<ſcript>ignored</ſcript><ſtyle>hidden</ſtyle>after",
        "<scripté>prose</script><script\u{301}>also prose</script>",
        "<script/>ignored</script><style!>hidden</style>after",
        "<script><style>hidden</style>visible",
        "<script <style>first</style>second</script>after",
        "\u{0085}é\u{2028}字\u{3000}",
    ] {
        let no_script = scripts.replace_all(body, " ");
        let no_tags = tags.replace_all(&no_script, " ");
        let expected = whitespace.replace_all(no_tags.trim(), " ");
        let actual = text_excerpt(body.as_bytes());
        assert_eq!(actual, expected, "{body:?}");
        assert_eq!(actual.capacity(), MAX_TEXT_EXCERPT_BYTES);
    }
    // A raw candidate buffer capped before whitespace normalization would
    // lose the terminal 'z'; it must remain visible for an unclosed tag.
    let body = format!("<{}z", " ".repeat(2 * MAX_TEXT_EXCERPT_BYTES));
    assert_eq!(text_excerpt(body.as_bytes()), "< z");
    let body = format!("x{}", "é".repeat(MAX_TEXT_EXCERPT_BYTES));
    let excerpt = text_excerpt(body.as_bytes());
    assert_eq!(excerpt.len(), MAX_TEXT_EXCERPT_BYTES - 1);
    assert_eq!(excerpt.capacity(), MAX_TEXT_EXCERPT_BYTES);
}

#[test]
fn bounded_text_scanner_matches_legacy_pipeline_for_malformed_utf8() {
    let scripts =
        Regex::new(r"(?is)<script\b[^>]*>.*?</script>|<style\b[^>]*>.*?</style>").unwrap();
    let tags = Regex::new(r"(?s)<[^>]+>").unwrap();
    let whitespace = Regex::new(r"\s+").unwrap();
    let compare = |raw: &[u8]| {
        let body = String::from_utf8_lossy(raw);
        let no_script = scripts.replace_all(&body, " ");
        let no_tags = tags.replace_all(&no_script, " ");
        let expected = whitespace.replace_all(no_tags.trim(), " ");
        assert_eq!(text_excerpt(raw), expected, "{raw:?}");
    };
    // Exercise every leading byte, incomplete valid sequences, invalid
    // continuation/overlong sequences and a replacement at the script-name
    // word boundary. These use the former full-decode/full-regex pipeline
    // as an independent semantic oracle on deliberately small inputs.
    for byte in 0..=u8::MAX {
        compare(&[b'<', b'p', b'>', byte, b'a', b'<', b'/', b'p', b'>']);
    }
    for raw in [
        b"\xf0\x90\x80z\xc2".as_slice(),
        b"\xf0\x90\x80\x80\xed\xa0\x80\xf4\x90\x80\x80",
        b"<script\xff>hidden</script>after",
        b"<style\xf0\x90>hidden</style>after",
        b"<p title='<script>\xff</script>'>visible</p><\xff tail",
    ] {
        for end in 0..=raw.len() {
            compare(&raw[..end]);
        }
    }
}

#[test]
fn bounded_text_scanner_stops_before_unused_tail() {
    for (prefix, expected) in [
        (
            vec![b'x'; MAX_TEXT_EXCERPT_BYTES],
            "x".repeat(MAX_TEXT_EXCERPT_BYTES),
        ),
        (vec![0xff; 66_667], "\u{fffd}".repeat(66_666)),
    ] {
        let prefix_len = prefix.len();
        let mut raw = b"<p>".to_vec();
        raw.extend_from_slice(&prefix);
        raw.extend_from_slice(b"</p><script>");
        raw.extend(std::iter::repeat_n(b'x', 4 * 1024 * 1024));
        raw.extend_from_slice(b"</script>\xff");
        let mut scanner = TextScanner::new(&raw);
        scanner.scan();
        assert_eq!(scanner.output.as_str(), expected);
        // Must fail if scanning continues after saturation. In particular,
        // neither the trailing script nor the unused malformed byte can
        // cause a full-body regex/UTF-8 pass before taking this excerpt.
        assert!(
            scanner.furthest_byte <= prefix_len + 16,
            "{}",
            scanner.furthest_byte
        );
        assert!(
            scanner.inspected_bytes <= 4 * prefix_len + 64,
            "{}",
            scanner.inspected_bytes
        );
    }
}

#[test]
fn bounded_text_scanner_does_not_rescan_unclosed_script_tails() {
    let body = format!("{}last", "<script><style>".repeat(256));
    let mut scanner = TextScanner::new(body.as_bytes());
    scanner.scan();
    assert_eq!(scanner.output.as_str(), "last");
    // Missing end tags require looking to EOF for legacy semantics, but
    // one remembered failed search per kind keeps repeated openers linear.
    // Must fail if the absent-closing-token cache is removed.
    assert!(
        scanner.inspected_bytes <= 8 * body.len(),
        "{}",
        scanner.inspected_bytes
    );
}

#[tokio::test]
async fn derived_admission_bounds_cancelled_waiters_and_releases_completed_leases() {
    use std::future::{poll_fn, Future};
    use std::task::Poll;

    assert_eq!(MAX_DERIVED_BYTES, 201_726_592);
    let admission = Arc::new(tokio::sync::Semaphore::new(MAX_DERIVED_BYTES));
    let maximum_decode = 3 * khive_storage::MAX_BLOB_WHOLE_BYTES as usize;
    let lease = admit_derived_buffers(&admission, maximum_decode)
        .await
        .unwrap();
    assert_eq!(admission.available_permits(), 0);
    let (cancel, cancelled) = tokio::sync::watch::channel(false);
    let waiting = khive_storage::scope_request_read_cancellation(
        cancelled,
        admit_derived_buffers(&admission, 0),
    );
    tokio::pin!(waiting);
    // Must fail if admission is bypassed or its lease is released before
    // parsing/persistence. One explicit poll proves blocking, without timing.
    poll_fn(|cx| {
        assert!(waiting.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    cancel.send(true).unwrap();
    let error = waiting.await.unwrap_err();
    assert!(matches!(error,
            RuntimeError::Storage(khive_storage::StorageError::Timeout { ref operation })
            if operation == "web_extract_derived_admission"));
    assert_eq!(admission.available_permits(), 0);
    drop(lease);
    assert_eq!(admission.available_permits(), MAX_DERIVED_BYTES);

    let lease = admit_derived_buffers(&admission, 0).await.unwrap();
    assert_eq!(
        admission.available_permits(),
        MAX_DERIVED_BYTES - TEXT_SCRATCH_BYTES
    );
    drop(lease);
    assert!(admit_derived_buffers(&admission, maximum_decode + 1)
        .await
        .is_err());
    assert_eq!(admission.available_permits(), MAX_DERIVED_BYTES);
}

#[derive(Debug)]
struct PausedPut {
    inner: Arc<dyn khive_storage::BlobStore>,
    started: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl khive_storage::BlobStore for PausedPut {
    async fn put(&self, bytes: Vec<u8>) -> khive_storage::StorageResult<khive_storage::ContentRef> {
        self.started.notify_one();
        self.release.acquire().await.unwrap().forget();
        self.inner.put(bytes).await
    }

    async fn get_bounded_verified(
        &self,
        id: &khive_storage::ContentRef,
        max: u64,
    ) -> khive_storage::StorageResult<Vec<u8>> {
        self.inner.get_bounded_verified(id, max).await
    }

    async fn exists(&self, id: &khive_storage::ContentRef) -> khive_storage::StorageResult<bool> {
        self.inner.exists(id).await
    }

    async fn size(
        &self,
        id: &khive_storage::ContentRef,
    ) -> khive_storage::StorageResult<Option<u64>> {
        self.inner.size(id).await
    }

    async fn delete(&self, id: &khive_storage::ContentRef) -> khive_storage::StorageResult<bool> {
        self.inner.delete(id).await
    }
}

#[tokio::test]
async fn cancelled_text_put_keeps_admission_until_background_io_finishes() {
    use std::future::{poll_fn, Future};
    use std::task::Poll;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(PausedPut {
        inner: Arc::new(
            khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0).unwrap(),
        ),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let admission = Arc::new(tokio::sync::Semaphore::new(TEXT_SCRATCH_BYTES));
    let permit = Arc::new(admit_derived_buffers(&admission, 0).await.unwrap());
    let (cancel, cancelled) = tokio::sync::watch::channel(false);
    let writing = khive_storage::scope_request_read_cancellation(
        cancelled,
        put_excerpt(store.clone(), "excerpt".into(), permit),
    );
    tokio::pin!(writing);
    // The notification is the assertion boundary; the timeout only catches
    // a hung test. No elapsed-time assumption decides admission correctness.
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = &mut writing => panic!("put missed the barrier: {result:?}"),
            _ = store.started.notified() => {},
        }
    })
    .await
    .unwrap();
    cancel.send(true).unwrap();
    let error = writing.await.unwrap_err();
    assert!(matches!(error,
            RuntimeError::Storage(khive_storage::StorageError::Timeout { ref operation })
            if operation == "web_extract_text_put"));
    // Must fail if put is awaited in the cancelled request without a
    // supervisor retaining the derived lease alongside its owned bytes.
    assert_eq!(admission.available_permits(), 0);
    let next = admit_derived_buffers(&admission, 0);
    tokio::pin!(next);
    poll_fn(|cx| {
        assert!(next.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    store.release.add_permits(1);
    let next_lease = tokio::time::timeout(Duration::from_secs(5), &mut next)
        .await
        .unwrap()
        .unwrap();
    drop(next_lease);
    assert_eq!(admission.available_permits(), TEXT_SCRATCH_BYTES);
    let content_ref =
        khive_storage::ContentRef::from_digest_bytes(blake3::hash(b"excerpt").as_bytes());
    assert!(store.inner.exists(&content_ref).await.unwrap());
}

/// See `fetch::tests::install_web_edge_rules` for why this is needed:
/// the in-crate test runtime carries no `VerbRegistry`, so the web
/// pack's own `EDGE_RULES` are never installed on it by default.
fn install_web_edge_rules(runtime: &KhiveRuntime) {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(crate::WebPack::new(runtime.clone()));
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

async fn seed_page(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    url_str: &str,
    content_type: &str,
    body: &[u8],
) -> Uuid {
    let url = Url::parse(url_str).unwrap();
    let canonical = identity::canonicalize(url);
    let site = identity::site_id(&khive_types::Namespace::local(), &canonical);
    crate::entities::get_or_create(
        runtime,
        token,
        site,
        "service",
        "site",
        &identity::site_key(&canonical),
        json!({ "scheme": canonical.scheme(), "host": canonical.host_str() }),
    )
    .await
    .unwrap();
    let id = identity::document_id(site, &identity::path_and_query(&canonical));
    let store = runtime.require_blob_store().unwrap();
    let content_ref = store.put(body.to_vec()).await.unwrap();
    let entity_type = if content_type.starts_with("text/html") {
        "page"
    } else {
        "resource"
    };
    crate::entities::get_or_create(
        runtime,
        token,
        id,
        "document",
        entity_type,
        canonical.as_ref(),
        json!({ "url": canonical.to_string() }),
    )
    .await
    .unwrap();
    crate::entities::patch(
        runtime,
        token,
        id,
        Some(entity_type),
        json!({
            "url": canonical.to_string(),
            "content_type": content_type,
            "blob_ref": content_ref.to_string(),
        }),
    )
    .await
    .unwrap();
    crate::fetch::root_body(
        runtime,
        id,
        khive_storage::AttachmentSubstrate::Entity,
        &content_ref,
        Some(content_type),
        body.len() as u64,
    )
    .await
    .unwrap();
    id
}

async fn capture_page(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    body: &[u8],
    link_headers: &[&str],
) -> (String, Uuid) {
    let store = runtime.require_blob_store().unwrap();
    let content_ref = store.put(body.to_vec()).await.unwrap();
    let reference = content_ref.to_string();
    crate::entities::patch(
        runtime,
        token,
        id,
        None,
        json!({ "blob_ref": reference, "content_type": "text/html" }),
    )
    .await
    .unwrap();
    crate::fetch::root_body(
        runtime,
        id,
        khive_storage::AttachmentSubstrate::Entity,
        &content_ref,
        Some("text/html"),
        body.len() as u64,
    )
    .await
    .unwrap();
    let receipt_id = crate::receipt::write_receipt(
        runtime,
        token,
        "web.fetch GET test capture",
        json!({
            "verb": "web.fetch",
            "content_ref": reference,
            "body_entity_id": id.to_string(),
            "headers": { "link": link_headers },
        }),
        vec![id],
    )
    .await
    .unwrap();
    crate::entities::patch(
        runtime,
        token,
        id,
        None,
        json!({ "capture_receipt_id": receipt_id.to_string() }),
    )
    .await
    .unwrap();
    (reference, receipt_id)
}

// A2: extract(links) on a page with N distinct hrefs yields N links_to
// edges whose targets are minted as unfetched resources; a repeated
// href is not double-counted (dedup), and a fragment-only href is
// skipped as not a distinct resource.
#[tokio::test]
async fn a2_extract_links_yields_n_edges_to_unfetched_resources_dedup_and_fragment_skip() {
    let (runtime, token, _dir) = test_runtime().await;
    let html = br##"<html><body>
            <a href="/a">A</a>
            <a href="/b">B</a>
            <a href="/a">A again</a>
            <a href="#top">fragment only</a>
            <a href="https://other.example.test/c">C</a>
        </body></html>"##;
    let page_id = seed_page(
        &runtime,
        &token,
        "https://origin.example.test/",
        "text/html",
        html,
    )
    .await;

    let reply = run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["links".to_string()]),
            link_limit: None,
            namespace: None,
        },
    )
    .await
    .expect("extract succeeds");
    assert_eq!(
        reply["result"]["links"]["edges_created"], 3,
        "a, b, c — deduped, fragment skipped"
    );

    let neighbors = runtime
        .neighbors(
            &token,
            page_id,
            khive_storage::Direction::Out,
            None,
            Some(vec![EdgeRelation::LinksTo]),
        )
        .await
        .unwrap();
    assert_eq!(neighbors.len(), 3);
    for n in &neighbors {
        let entity = runtime
            .entities(&token)
            .unwrap()
            .get_entity(n.node_id)
            .await
            .unwrap()
            .expect("target minted");
        assert_eq!(
            entity.entity_type.as_deref(),
            Some("resource"),
            "unfetched target starts as resource"
        );
        assert_eq!(entity.properties.unwrap()["status"], Value::Null);
    }
}

// Set-like kind selection must not hide work committed by an earlier pass.
#[tokio::test]
async fn duplicate_sitemap_kind_preserves_admitted_count() {
    for repetitions in [1usize, 2] {
        let (runtime, token, _dir) = test_runtime().await;
        let document = seed_page(
            &runtime,
            &token,
            "https://duplicate-kind.example.test/map.xml",
            "application/xml",
            b"<urlset><url><loc>https://duplicate-kind.example.test/entry</loc></url></urlset>",
        )
        .await;
        let kinds = vec!["sitemap"; repetitions];
        let pack = crate::WebPack::new(runtime.clone());
        let reply = pack
            .handle_extract(
                &token,
                json!({ "id": document, "kinds": kinds, "link_limit": 1 }),
            )
            .await
            .unwrap();
        let site = identity::site_id(
            &khive_types::Namespace::local(),
            &Url::parse("https://duplicate-kind.example.test/map.xml").unwrap(),
        );
        let neighbors = runtime
            .neighbors(
                &token,
                site,
                khive_storage::Direction::Out,
                None,
                Some(vec![EdgeRelation::Contains]),
            )
            .await
            .unwrap();
        assert_eq!(
            neighbors.len(),
            1,
            "the admitted target remains in the graph"
        );
        assert_eq!(
            reply["result"]["sitemap"]["entries"], 1,
            "duplicate kind must not overwrite earlier admitted work with zero"
        );
    }
}

#[tokio::test]
async fn sitemap_and_feed_share_a_bounded_entry_budget_and_report_skips() {
    let (runtime, token, _dir) = test_runtime().await;
    let mut body = String::from("<urlset>");
    for index in 0..5_000 {
        body.push_str(&format!(
            "<url><loc>https://entries.example.test/{index}</loc></url>"
        ));
    }
    body.push_str("<link href=\"https://entries.example.test/feed-a\"/><link href=\"https://entries.example.test/feed-b\"/></urlset>");
    let document = seed_page(
        &runtime,
        &token,
        "https://publisher.example.test/map.xml",
        "application/xml",
        body.as_bytes(),
    )
    .await;

    let reply = run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(document),
            url: None,
            kinds: Some(vec!["sitemap".into(), "feed".into()]),
            link_limit: Some(3),
            namespace: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(reply["result"]["sitemap"]["entries"], 3);
    assert_eq!(reply["result"]["sitemap"]["skipped"], 4_997);
    assert_eq!(reply["result"]["feed"]["entries"], 0);
    assert_eq!(reply["result"]["feed"]["skipped"], 2);

    let site = identity::site_id(
        &khive_types::Namespace::local(),
        &Url::parse("https://publisher.example.test/map.xml").unwrap(),
    );
    let neighbors = runtime
        .neighbors(
            &token,
            site,
            khive_storage::Direction::Out,
            None,
            Some(vec![EdgeRelation::Contains]),
        )
        .await
        .unwrap();
    assert_eq!(
        neighbors.len(),
        3,
        "only admitted entries acquire graph edges"
    );
    let fourth = Url::parse("https://entries.example.test/3").unwrap();
    let fourth_id = identity::document_id(
        identity::site_id(&khive_types::Namespace::local(), &fourth),
        &identity::path_and_query(&fourth),
    );
    assert!(runtime
        .entities(&token)
        .unwrap()
        .get_entity(fourth_id)
        .await
        .unwrap()
        .is_none());
}

// extract(text) mints a derived_from resource holding tag-stripped
// text, and repeating the call converges on the same id.
#[tokio::test]
async fn extract_text_mints_derived_from_resource_idempotent_id() {
    let (runtime, token, _dir) = test_runtime().await;
    let html = b"<html><body><p>Hello   world</p><script>ignored();</script></body></html>";
    let page_id = seed_page(
        &runtime,
        &token,
        "https://origin.example.test/page",
        "text/html",
        html,
    )
    .await;

    let reply1 = run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["text".to_string()]),
            link_limit: None,
            namespace: None,
        },
    )
    .await
    .unwrap();
    let text_id_1 = reply1["result"]["text"]["id"].as_str().unwrap().to_string();

    let reply2 = run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["text".to_string()]),
            link_limit: None,
            namespace: None,
        },
    )
    .await
    .unwrap();
    let text_id_2 = reply2["result"]["text"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        text_id_1, text_id_2,
        "repeated extraction converges on one id"
    );

    let entity = runtime
        .entities(&token)
        .unwrap()
        .get_entity(uuid::Uuid::parse_str(&text_id_1).unwrap())
        .await
        .unwrap()
        .unwrap();
    let store = runtime.require_blob_store().unwrap();
    let content_ref = khive_storage::ContentRef::from_hex(
        entity.properties.unwrap()["blob_ref"].as_str().unwrap(),
    )
    .unwrap();
    let bytes = store
        .get_bounded_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await
        .unwrap();
    let roots = runtime
        .core()
        .attachments()
        .unwrap()
        .list_attachments(entity.id)
        .await
        .unwrap();
    assert_eq!(
        roots.len(),
        1,
        "repeated extraction retains one content root"
    );
    assert_eq!(roots[0].role, "content");
    assert_eq!(roots[0].content_ref, content_ref);
    assert_eq!(roots[0].media_type.as_deref(), Some("text/plain"));
    assert_eq!(roots[0].size_bytes, Some(bytes.len() as u64));
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("Hello world"), "{text:?}");
    assert!(
        !text.contains("ignored"),
        "script content is stripped like any other tag body"
    );

    let neighbors = runtime
        .neighbors(
            &token,
            uuid::Uuid::parse_str(&text_id_1).unwrap(),
            khive_storage::Direction::Out,
            None,
            Some(vec![EdgeRelation::DerivedFrom]),
        )
        .await
        .unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0].node_id, page_id);
}

#[tokio::test]
async fn two_captures_keep_distinct_excerpts_and_retract_missing_live_links() {
    let (runtime, token, _dir) = test_runtime().await;
    let first = b"<p>Same words</p><a href='/old' rel='next'>Link</a>";
    let second = b"<section>Same words</section><a href='/new' rel='next'>Link</a>";
    let page_id = seed_page(
        &runtime,
        &token,
        "https://capture.example.test/page",
        "text/html",
        first,
    )
    .await;
    let (first_ref, first_capture) = capture_page(&runtime, &token, page_id, first, &[]).await;
    crate::receipt::write_receipt(
        &runtime,
        &token,
        "web.fetch HEAD after test capture",
        json!({ "verb": "web.fetch", "method": "HEAD", "content_ref": null }),
        vec![page_id],
    )
    .await
    .unwrap();
    let params = || ExtractParams {
        id: Some(page_id),
        url: None,
        kinds: Some(vec!["text".into(), "links".into()]),
        link_limit: None,
        namespace: None,
    };
    let first_reply = run_extract(&runtime, &token, params()).await.unwrap();
    let first_text =
        Uuid::parse_str(first_reply["result"]["text"]["id"].as_str().unwrap()).unwrap();
    let first_extraction = Uuid::parse_str(first_reply["receipt_id"].as_str().unwrap()).unwrap();

    let (second_ref, second_capture) = capture_page(&runtime, &token, page_id, second, &[]).await;
    let second_reply = run_extract(&runtime, &token, params()).await.unwrap();
    let second_text =
        Uuid::parse_str(second_reply["result"]["text"]["id"].as_str().unwrap()).unwrap();
    let second_extraction = Uuid::parse_str(second_reply["receipt_id"].as_str().unwrap()).unwrap();

    assert_ne!(first_ref, second_ref);
    assert_ne!(
        first_text, second_text,
        "body identity must not be excerpt identity"
    );
    let notes = runtime.notes(&token).unwrap();
    for (id, expected_ref, expected_capture, expected_target) in [
        (first_extraction, &first_ref, first_capture, "/old"),
        (second_extraction, &second_ref, second_capture, "/new"),
    ] {
        let note = notes.get_note(id).await.unwrap().unwrap();
        let properties = note.properties.unwrap();
        let request = &properties["request"];
        assert_eq!(request["source_content_ref"], expected_ref.as_str());
        assert_eq!(request["capture_receipt_id"], expected_capture.to_string());
        assert!(request["links"][0]["url"]
            .as_str()
            .unwrap()
            .ends_with(expected_target));
        let source = runtime
            .core()
            .attachments()
            .unwrap()
            .get_attachment(id, "source")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source.content_ref.to_string(), expected_ref.as_str());
    }
    let entities = runtime.entities(&token).unwrap();
    let old_text = entities.get_entity(first_text).await.unwrap().unwrap();
    let new_text = entities.get_entity(second_text).await.unwrap().unwrap();
    assert_eq!(
        old_text.properties.as_ref().unwrap()["source_content_ref"],
        first_ref
    );
    assert_eq!(
        new_text.properties.as_ref().unwrap()["source_content_ref"],
        second_ref
    );
    assert_eq!(
        old_text.properties.as_ref().unwrap()["blob_ref"],
        new_text.properties.as_ref().unwrap()["blob_ref"],
        "two different HTML bodies can produce the same excerpt bytes"
    );
    let neighbors = runtime
        .neighbors(
            &token,
            page_id,
            khive_storage::Direction::Out,
            None,
            Some(vec![EdgeRelation::LinksTo]),
        )
        .await
        .unwrap();
    assert_eq!(neighbors.len(), 1);
    let current = entities
        .get_entity(neighbors[0].node_id)
        .await
        .unwrap()
        .unwrap();
    assert!(current.name.ends_with("/new"));

    capture_page(&runtime, &token, page_id, first, &[]).await;
    run_extract(&runtime, &token, params()).await.unwrap();
    let restored = runtime
        .neighbors(
            &token,
            page_id,
            khive_storage::Direction::Out,
            None,
            Some(vec![EdgeRelation::LinksTo]),
        )
        .await
        .unwrap();
    assert_eq!(restored.len(), 1);
    let restored_target = entities
        .get_entity(restored[0].node_id)
        .await
        .unwrap()
        .unwrap();
    assert!(restored_target.name.ends_with("/old"));
}

#[tokio::test]
async fn link_rel_context_occurrences_and_header_survive_on_edges_and_receipt() {
    let (runtime, token, _dir) = test_runtime().await;
    let body = br#"<a href="/same" rel="next prev">Continue</a>
            <a href="/same" rel="license">Terms</a>
            <link href="/style.css" rel="stylesheet" title="Main style">"#;
    let page_id = seed_page(
        &runtime,
        &token,
        "https://links.example.test/page",
        "text/html",
        body,
    )
    .await;
    capture_page(
        &runtime,
        &token,
        page_id,
        body,
        &["<https://links.example.test/legal>; rel=license; title=Legal"],
    )
    .await;
    let reply = run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["links".into()]),
            link_limit: None,
            namespace: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(reply["result"]["links"]["edges_created"], 3);
    let edges = runtime
        .list_edges(
            &token,
            EdgeListFilter {
                source_id: Some(page_id),
                relations: vec![EdgeRelation::LinksTo],
                ..Default::default()
            },
            100,
            0,
        )
        .await
        .unwrap();
    assert_eq!(edges.len(), 3);
    let same = identity::canonicalize(Url::parse("https://links.example.test/same").unwrap());
    let same_id = identity::document_id(
        identity::site_id(&khive_types::Namespace::local(), &same),
        &identity::path_and_query(&same),
    );
    let same_edge = edges.iter().find(|edge| edge.target_id == same_id).unwrap();
    let metadata = same_edge.metadata.as_ref().unwrap();
    assert_eq!(metadata["occurrence_count"], 2);
    assert_eq!(metadata["occurrences"][0]["rel"], json!(["next", "prev"]));
    assert_eq!(metadata["occurrences"][0]["context"], "Continue");
    assert_eq!(metadata["occurrences"][1]["rel"], json!(["license"]));
    assert_eq!(metadata["occurrences"][1]["context"], "Terms");
    assert!(edges.iter().any(
        |edge| edge.metadata.as_ref().unwrap()["occurrences"][0]["rel"] == json!(["stylesheet"])
    ));
    assert!(edges
        .iter()
        .any(|edge| edge.metadata.as_ref().unwrap()["occurrences"][0]["source"] == "header"));

    let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
    let receipt = runtime
        .notes(&token)
        .unwrap()
        .get_note(receipt_id)
        .await
        .unwrap()
        .unwrap();
    let properties = receipt.properties.unwrap();
    let request = &properties["request"];
    assert_eq!(request["links"].as_array().unwrap().len(), 3);
    assert_eq!(request["links_complete"], true);

    let limited = run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["links".into()]),
            link_limit: Some(1),
            namespace: None,
        },
    )
    .await
    .unwrap();
    let live = runtime
        .list_edges(
            &token,
            EdgeListFilter {
                source_id: Some(page_id),
                relations: vec![EdgeRelation::LinksTo],
                ..Default::default()
            },
            100,
            0,
        )
        .await
        .unwrap();
    assert_eq!(live.len(), 3, "budget-skipped present links stay live");
    assert!(live.iter().any(|edge| edge.target_id == same_id));
    let limited_receipt = runtime
        .notes(&token)
        .unwrap()
        .get_note(Uuid::parse_str(limited["receipt_id"].as_str().unwrap()).unwrap())
        .await
        .unwrap()
        .unwrap();
    let limited_properties = limited_receipt.properties.unwrap();
    let limited_request = &limited_properties["request"];
    assert_eq!(limited_request["links_complete"], false);
    assert_eq!(limited_request["links"].as_array().unwrap().len(), 3);
    assert_eq!(limited_request["links"][1]["admitted"], false);
    assert!(limited_request["links"][1]["edge_id"].is_string());
}

#[tokio::test]
async fn legacy_links_are_preserved_with_present_collision_and_absence_evidence() {
    let (runtime, token, _dir) = test_runtime().await;
    let first_body = b"<a href='/present'>Present</a><a href='/deleted'>Deleted</a>";
    let page_id = seed_page(
        &runtime,
        &token,
        "https://legacy.example.test/page",
        "text/html",
        first_body,
    )
    .await;
    let present_id = seed_page(
        &runtime,
        &token,
        "https://legacy.example.test/present",
        "text/html",
        b"<p>Target</p>",
    )
    .await;
    let absent_id = seed_page(
        &runtime,
        &token,
        "https://legacy.example.test/absent",
        "text/html",
        b"<p>Target</p>",
    )
    .await;
    let deleted_id = seed_page(
        &runtime,
        &token,
        "https://legacy.example.test/deleted",
        "text/html",
        b"<p>Target</p>",
    )
    .await;
    runtime
        .link(
            &token,
            page_id,
            present_id,
            EdgeRelation::LinksTo,
            0.7,
            Some(json!({ "legacy_label": "kept" })),
        )
        .await
        .unwrap();
    runtime
        .link(&token, page_id, absent_id, EdgeRelation::LinksTo, 1.0, None)
        .await
        .unwrap();
    let deleted_edge = runtime
        .link(
            &token,
            page_id,
            deleted_id,
            EdgeRelation::LinksTo,
            1.0,
            None,
        )
        .await
        .unwrap();
    runtime
        .delete_edge(&token, Uuid::from(deleted_edge.id), false)
        .await
        .unwrap();
    capture_page(&runtime, &token, page_id, first_body, &[]).await;
    let params = || ExtractParams {
        id: Some(page_id),
        url: None,
        kinds: Some(vec!["links".into()]),
        link_limit: Some(2),
        namespace: None,
    };
    let first = run_extract(&runtime, &token, params()).await.unwrap();
    assert_eq!(first["result"]["links"]["edges_created"], 0);
    assert_eq!(first["result"]["links"]["admitted_targets"], 2);
    assert_eq!(first["result"]["links"]["ownership_collisions"], 2);
    let filter = EdgeListFilter {
        source_id: Some(page_id),
        relations: vec![EdgeRelation::LinksTo],
        ..Default::default()
    };
    let edges = runtime
        .list_edges(&token, filter.clone(), 100, 0)
        .await
        .unwrap();
    assert_eq!(edges.len(), 2);
    let present_edge = edges
        .iter()
        .find(|edge| edge.target_id == present_id)
        .unwrap();
    assert_eq!(
        present_edge.metadata.as_ref().unwrap(),
        &json!({ "legacy_label": "kept" })
    );
    assert_eq!(present_edge.weight, 0.7);
    let absent_edge = edges
        .iter()
        .find(|edge| edge.target_id == absent_id)
        .unwrap();
    assert!(absent_edge.metadata.is_none());
    let first_receipt = runtime
        .notes(&token)
        .unwrap()
        .get_note(Uuid::parse_str(first["receipt_id"].as_str().unwrap()).unwrap())
        .await
        .unwrap()
        .unwrap();
    let first_properties = first_receipt.properties.unwrap();
    let first_request = &first_properties["request"];
    assert_eq!(first_request["links"][0]["admitted"], true);
    assert_eq!(first_request["links"][0]["ownership_collision"], true);
    assert_eq!(first_request["links"][1]["ownership_collision"], true);
    assert_eq!(first_request["links_complete"], false);
    assert!(first_request["legacy_claimed"]
        .as_array()
        .unwrap()
        .is_empty());
    let unclaimed = first_request["legacy_unclaimed"].as_array().unwrap();
    assert_eq!(unclaimed.len(), 3);
    assert!(unclaimed.iter().any(|edge| {
        edge["target_id"] == present_id.to_string()
            && edge["present_in_extraction"] == true
            && edge["collides_with_admitted_target"] == true
    }));
    assert!(unclaimed.iter().any(|edge| {
        edge["target_id"] == absent_id.to_string() && edge["present_in_extraction"] == false
    }));
    assert!(unclaimed.iter().any(|edge| {
        edge["target_id"] == deleted_id.to_string()
            && edge["live"] == false
            && edge["collides_with_admitted_target"] == true
    }));
    let tombstone = runtime
        .get_edge_by_natural_key_including_deleted(
            &token,
            token.namespace().as_str(),
            page_id,
            deleted_id,
            EdgeRelation::LinksTo,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(tombstone.deleted_at.is_some());

    capture_page(&runtime, &token, page_id, b"<p>No links</p>", &[]).await;
    let second = run_extract(&runtime, &token, params()).await.unwrap();
    let edges = runtime.list_edges(&token, filter, 100, 0).await.unwrap();
    assert_eq!(edges.len(), 2);
    assert!(edges.iter().any(|edge| {
        edge.target_id == present_id
            && edge.metadata.as_ref() == Some(&json!({ "legacy_label": "kept" }))
            && edge.weight == 0.7
    }));
    assert!(edges
        .iter()
        .any(|edge| edge.target_id == absent_id && edge.metadata.is_none()));
    let second_receipt = runtime
        .notes(&token)
        .unwrap()
        .get_note(Uuid::parse_str(second["receipt_id"].as_str().unwrap()).unwrap())
        .await
        .unwrap()
        .unwrap();
    let second_properties = second_receipt.properties.unwrap();
    assert_eq!(
        second_properties["request"]["legacy_unclaimed"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn legacy_default_present_is_claimed_caller_metadata_collides_and_absent_survives() {
    let (runtime, token, _dir) = test_runtime().await;
    let first_body = b"<a href='/default'>Default</a><a href='/caller'>Caller</a>";
    let page_id = seed_page(
        &runtime,
        &token,
        "https://claim.example.test/page",
        "text/html",
        first_body,
    )
    .await;
    let default_id = seed_page(
        &runtime,
        &token,
        "https://claim.example.test/default",
        "text/html",
        b"<p>Default</p>",
    )
    .await;
    let caller_id = seed_page(
        &runtime,
        &token,
        "https://claim.example.test/caller",
        "text/html",
        b"<p>Caller</p>",
    )
    .await;
    let absent_id = seed_page(
        &runtime,
        &token,
        "https://claim.example.test/absent",
        "text/html",
        b"<p>Absent</p>",
    )
    .await;
    let original_default = runtime
        .link(
            &token,
            page_id,
            default_id,
            EdgeRelation::LinksTo,
            1.0,
            None,
        )
        .await
        .unwrap();
    runtime
        .link(
            &token,
            page_id,
            caller_id,
            EdgeRelation::LinksTo,
            1.0,
            Some(json!({ "caller_label": "keep" })),
        )
        .await
        .unwrap();
    runtime
        .link(&token, page_id, absent_id, EdgeRelation::LinksTo, 1.0, None)
        .await
        .unwrap();
    capture_page(&runtime, &token, page_id, first_body, &[]).await;
    let params = || ExtractParams {
        id: Some(page_id),
        url: None,
        kinds: Some(vec!["links".into()]),
        link_limit: Some(2),
        namespace: None,
    };
    let first = run_extract(&runtime, &token, params()).await.unwrap();
    assert_eq!(first["result"]["links"]["edges_created"], 1);
    assert_eq!(first["result"]["links"]["admitted_targets"], 2);
    assert_eq!(first["result"]["links"]["ownership_collisions"], 1);
    let filter = EdgeListFilter {
        source_id: Some(page_id),
        relations: vec![EdgeRelation::LinksTo],
        ..Default::default()
    };
    let edges = runtime
        .list_edges(&token, filter.clone(), 100, 0)
        .await
        .unwrap();
    assert_eq!(edges.len(), 3);
    let default = edges
        .iter()
        .find(|edge| edge.target_id == default_id)
        .unwrap();
    assert_eq!(default.id, original_default.id);
    assert_eq!(default.metadata.as_ref().unwrap()["web_extract"], true);
    let caller = edges
        .iter()
        .find(|edge| edge.target_id == caller_id)
        .unwrap();
    assert_eq!(
        caller.metadata.as_ref().unwrap(),
        &json!({ "caller_label": "keep" })
    );
    let absent = edges
        .iter()
        .find(|edge| edge.target_id == absent_id)
        .unwrap();
    assert!(absent.metadata.is_none());
    let receipt = runtime
        .notes(&token)
        .unwrap()
        .get_note(Uuid::parse_str(first["receipt_id"].as_str().unwrap()).unwrap())
        .await
        .unwrap()
        .unwrap();
    let receipt_properties = receipt.properties.unwrap();
    let request = &receipt_properties["request"];
    assert_eq!(request["links_complete"], false);
    let claimed = request["legacy_claimed"].as_array().unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        claimed[0]["edge_id"],
        Uuid::from(original_default.id).to_string()
    );
    assert_eq!(claimed[0]["target_id"], default_id.to_string());
    assert_eq!(claimed[0]["admitted"], true);
    let unclaimed = request["legacy_unclaimed"].as_array().unwrap();
    assert_eq!(unclaimed.len(), 2);
    assert!(unclaimed.iter().any(|edge| {
        edge["target_id"] == caller_id.to_string()
            && edge["present_in_extraction"] == true
            && edge["collides_with_admitted_target"] == true
    }));
    assert!(unclaimed.iter().any(|edge| {
        edge["target_id"] == absent_id.to_string() && edge["present_in_extraction"] == false
    }));

    capture_page(&runtime, &token, page_id, b"<p>No links</p>", &[]).await;
    run_extract(&runtime, &token, params()).await.unwrap();
    let edges = runtime.list_edges(&token, filter, 100, 0).await.unwrap();
    assert_eq!(edges.len(), 2);
    assert!(edges.iter().all(|edge| edge.target_id != default_id));
    assert!(edges.iter().any(|edge| {
        edge.target_id == caller_id
            && edge.metadata.as_ref() == Some(&json!({ "caller_label": "keep" }))
    }));
    assert!(edges
        .iter()
        .any(|edge| edge.target_id == absent_id && edge.metadata.is_none()));
}

#[tokio::test]
async fn legacy_default_present_is_claimed_even_when_budget_skips_target() {
    let (runtime, token, _dir) = test_runtime().await;
    let first_body = b"<a href='/target'>Target</a>";
    let page_id = seed_page(
        &runtime,
        &token,
        "https://claim.example.test/limited",
        "text/html",
        first_body,
    )
    .await;
    let target_id = seed_page(
        &runtime,
        &token,
        "https://claim.example.test/target",
        "text/html",
        b"<p>Target</p>",
    )
    .await;
    runtime
        .link(&token, page_id, target_id, EdgeRelation::LinksTo, 1.0, None)
        .await
        .unwrap();
    capture_page(&runtime, &token, page_id, first_body, &[]).await;
    let params = || ExtractParams {
        id: Some(page_id),
        url: None,
        kinds: Some(vec!["links".into()]),
        link_limit: Some(0),
        namespace: None,
    };
    let first = run_extract(&runtime, &token, params()).await.unwrap();
    assert_eq!(first["result"]["links"]["admitted_targets"], 0);
    assert_eq!(first["result"]["links"]["skipped"], 1);
    let receipt = runtime
        .notes(&token)
        .unwrap()
        .get_note(Uuid::parse_str(first["receipt_id"].as_str().unwrap()).unwrap())
        .await
        .unwrap()
        .unwrap();
    let receipt_properties = receipt.properties.unwrap();
    let request = &receipt_properties["request"];
    assert_eq!(
        request["legacy_claimed"][0]["target_id"],
        target_id.to_string()
    );
    assert_eq!(request["legacy_claimed"][0]["admitted"], false);
    let edge = runtime
        .list_edges(
            &token,
            EdgeListFilter {
                source_id: Some(page_id),
                target_id: Some(target_id),
                relations: vec![EdgeRelation::LinksTo],
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(edge.metadata.as_ref().unwrap()["web_extract"], true);
    capture_page(&runtime, &token, page_id, b"<p>No links</p>", &[]).await;
    run_extract(&runtime, &token, params()).await.unwrap();
    assert!(runtime
        .list_edges(
            &token,
            EdgeListFilter {
                source_id: Some(page_id),
                target_id: Some(target_id),
                relations: vec![EdgeRelation::LinksTo],
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn legacy_nondefault_weight_present_remains_unclaimed() {
    let (runtime, token, _dir) = test_runtime().await;
    let body = b"<a href='/weighted-target'>Target</a>";
    let page_id = seed_page(
        &runtime,
        &token,
        "https://claim.example.test/weighted",
        "text/html",
        body,
    )
    .await;
    let target_id = seed_page(
        &runtime,
        &token,
        "https://claim.example.test/weighted-target",
        "text/html",
        b"<p>Target</p>",
    )
    .await;
    runtime
        .link(&token, page_id, target_id, EdgeRelation::LinksTo, 0.7, None)
        .await
        .unwrap();
    capture_page(&runtime, &token, page_id, body, &[]).await;
    let reply = run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["links".into()]),
            link_limit: Some(1),
            namespace: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(reply["result"]["links"]["ownership_collisions"], 1);
    let edges = runtime
        .list_edges(
            &token,
            EdgeListFilter {
                source_id: Some(page_id),
                relations: vec![EdgeRelation::LinksTo],
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].weight, 0.7);
    assert!(edges[0].metadata.is_none());
    capture_page(&runtime, &token, page_id, b"<p>No links</p>", &[]).await;
    run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["links".into()]),
            link_limit: Some(1),
            namespace: None,
        },
    )
    .await
    .unwrap();
    let edges = runtime
        .list_edges(
            &token,
            EdgeListFilter {
                source_id: Some(page_id),
                relations: vec![EdgeRelation::LinksTo],
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].weight, 0.7);
    assert!(edges[0].metadata.is_none());
}

#[tokio::test]
async fn capture_selection_requires_the_body_owner_even_when_digest_and_annotation_match() {
    let (runtime, token, _dir) = test_runtime().await;
    let body = b"<p>Shared bytes</p>";
    let page_id = seed_page(
        &runtime,
        &token,
        "https://owner.example.test/page",
        "text/html",
        body,
    )
    .await;
    let (reference, source_capture) = capture_page(&runtime, &token, page_id, body, &[]).await;
    let other_id = seed_page(
        &runtime,
        &token,
        "https://owner.example.test/other",
        "text/html",
        body,
    )
    .await;
    let other_capture = crate::receipt::write_receipt(
        &runtime,
        &token,
        "redirect participant with identical body digest",
        json!({
            "verb": "web.fetch",
            "content_ref": reference,
            "body_entity_id": other_id.to_string(),
            "headers": { "link": ["<https://owner.example.test/wrong>; rel=next"] },
        }),
        vec![page_id, other_id],
    )
    .await
    .unwrap();
    let page = runtime
        .entities(&token)
        .unwrap()
        .get_entity(page_id)
        .await
        .unwrap()
        .unwrap();
    let selected = crate::receipt::capture_for_body(&runtime, &token, &page, &reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(selected.0, source_capture);

    // The pointer is only a hint. If it too names the wrong owner, the
    // capture is unknown rather than misattributed to this document.
    crate::entities::patch(
        &runtime,
        &token,
        page_id,
        None,
        json!({ "capture_receipt_id": other_capture.to_string() }),
    )
    .await
    .unwrap();
    let page = runtime
        .entities(&token)
        .unwrap()
        .get_entity(page_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        crate::receipt::capture_for_body(&runtime, &token, &page, &reference)
            .await
            .unwrap()
            .is_none()
    );
    let reply = run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["links".into()]),
            link_limit: Some(10),
            namespace: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(reply["result"]["links"]["edges_created"], 0);
    let receipt = runtime
        .notes(&token)
        .unwrap()
        .get_note(Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(receipt.properties.unwrap()["request"]["capture_receipt_id"].is_null());
}

#[tokio::test]
async fn legacy_unmarked_capture_extracts_without_receipt_or_header_links() {
    let (runtime, token, _dir) = test_runtime().await;
    let body = b"<p>Legacy body without HTML links</p>";
    let page_id = seed_page(
        &runtime,
        &token,
        "https://legacy-capture.example.test/page",
        "text/html",
        body,
    )
    .await;
    let page = runtime
        .entities(&token)
        .unwrap()
        .get_entity(page_id)
        .await
        .unwrap()
        .unwrap();
    let reference = page.properties.as_ref().unwrap()["blob_ref"]
        .as_str()
        .unwrap()
        .to_string();
    let legacy = runtime
        .create_note(
            &token,
            "observation",
            None,
            "pre-upgrade web receipt",
            None,
            Some(json!({
                "tags": [crate::receipt::RECEIPT_TAG],
                "request": {
                    "verb": "web.fetch",
                    "content_ref": reference.clone(),
                    "body_entity_id": page_id.to_string(),
                    "headers": {"link": ["<https://legacy-capture.example.test/header>; rel=next"]},
                },
            })),
            vec![page_id],
        )
        .await
        .unwrap();
    crate::entities::patch(
        &runtime,
        &token,
        page_id,
        None,
        json!({ "capture_receipt_id": legacy.id.to_string() }),
    )
    .await
    .unwrap();
    let page = runtime
        .entities(&token)
        .unwrap()
        .get_entity(page_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        crate::receipt::capture_for_body(&runtime, &token, &page, &reference)
            .await
            .unwrap()
            .is_none()
    );

    let reply = run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["links".into()]),
            link_limit: Some(10),
            namespace: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(reply["result"]["links"]["edges_created"], 0);
    let extraction_note = runtime
        .notes(&token)
        .unwrap()
        .get_note(Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(extraction_note.properties.unwrap()["request"]["capture_receipt_id"].is_null());
    assert!(runtime
        .notes(&token)
        .unwrap()
        .get_note(legacy.id)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn extraction_uses_genuine_capture_behind_newer_tagged_decoy() {
    let (runtime, token, _dir) = test_runtime().await;
    let body = b"<p>Captured body</p>";
    let page_id = seed_page(
        &runtime,
        &token,
        "https://owner.example.test/tagged-decoy",
        "text/html",
        body,
    )
    .await;
    let (_reference, genuine) = capture_page(&runtime, &token, page_id, body, &[]).await;
    let genuine_note = runtime
        .notes(&token)
        .unwrap()
        .get_note(genuine)
        .await
        .unwrap()
        .unwrap();
    let decoy = runtime
        .create_note(
            &token,
            "observation",
            None,
            "caller-written tagged decoy",
            None,
            Some(json!({"tags": [crate::receipt::RECEIPT_TAG]})),
            vec![page_id],
        )
        .await
        .unwrap();
    let mut newer_decoy = decoy.clone();
    newer_decoy.created_at = genuine_note.created_at + 1;
    newer_decoy.updated_at = newer_decoy.created_at;
    runtime
        .backend()
        .notes()
        .unwrap()
        .upsert_note(newer_decoy)
        .await
        .unwrap();
    assert_eq!(
        runtime
            .latest_annotating_note(&token, page_id, "observation", crate::receipt::RECEIPT_TAG)
            .await
            .unwrap(),
        Some(decoy.id)
    );
    crate::entities::patch(
        &runtime,
        &token,
        page_id,
        None,
        json!({ "capture_receipt_id": null }),
    )
    .await
    .unwrap();

    let reply = run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["text".into()]),
            link_limit: None,
            namespace: None,
        },
    )
    .await
    .unwrap();
    let extraction_note = runtime
        .notes(&token)
        .unwrap()
        .get_note(Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        extraction_note.properties.unwrap()["request"]["capture_receipt_id"],
        genuine.to_string()
    );
}

#[tokio::test]
async fn extraction_refuses_property_and_content_attachment_mismatch_before_writes() {
    let (runtime, token, _dir) = test_runtime().await;
    let page_id = seed_page(
        &runtime,
        &token,
        "https://owner.example.test/mismatch",
        "text/html",
        b"<a href='/old'>Old</a>",
    )
    .await;
    let store = runtime.require_blob_store().unwrap();
    let second = store.put(b"<a href='/new'>New</a>".to_vec()).await.unwrap();
    crate::entities::patch(
        &runtime,
        &token,
        page_id,
        None,
        json!({ "blob_ref": second.to_string() }),
    )
    .await
    .unwrap();
    assert!(!crate::receipt::bind_capture_receipt(
        &runtime,
        &token,
        page_id,
        second.as_ref(),
        Uuid::new_v4(),
    )
    .await
    .unwrap());
    let page = runtime
        .entities(&token)
        .unwrap()
        .get_entity(page_id)
        .await
        .unwrap()
        .unwrap();
    assert!(page
        .properties
        .as_ref()
        .and_then(|properties| properties.get("capture_receipt_id"))
        .is_none());
    let error = run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["links".into(), "text".into()]),
            link_limit: None,
            namespace: None,
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, RuntimeError::InvalidInput(ref reason) if reason.starts_with("capture_changed:"))
    );
    assert!(runtime
        .latest_annotating_note(
            &token,
            page_id,
            "observation",
            crate::receipt::EXTRACTION_RECEIPT_TAG
        )
        .await
        .unwrap()
        .is_none());
    assert!(runtime
        .list_edges(
            &token,
            EdgeListFilter {
                source_id: Some(page_id),
                relations: vec![EdgeRelation::LinksTo],
                ..Default::default()
            },
            100,
            0,
        )
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn occurrence_limit_writes_degraded_receipt_without_link_mutation() {
    let (runtime, token, _dir) = test_runtime().await;
    let body = "<a href='/same'>Same</a>".repeat(MAX_LINK_OCCURRENCES + 1);
    let page_id = seed_page(
        &runtime,
        &token,
        "https://limit.example.test/page",
        "text/html",
        body.as_bytes(),
    )
    .await;
    let marked_id = seed_page(
        &runtime,
        &token,
        "https://limit.example.test/marked",
        "text/html",
        b"<p>Target</p>",
    )
    .await;
    let unmarked_id = seed_page(
        &runtime,
        &token,
        "https://limit.example.test/unmarked",
        "text/html",
        b"<p>Target</p>",
    )
    .await;
    runtime
        .link(
            &token,
            page_id,
            marked_id,
            EdgeRelation::LinksTo,
            1.0,
            Some(json!({ "web_extract": true })),
        )
        .await
        .unwrap();
    runtime
        .link(
            &token,
            page_id,
            unmarked_id,
            EdgeRelation::LinksTo,
            1.0,
            None,
        )
        .await
        .unwrap();
    let (source_ref, _) = capture_page(&runtime, &token, page_id, body.as_bytes(), &[]).await;
    let reply = run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["links".into(), "text".into()]),
            link_limit: Some(1),
            namespace: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(reply["status"], "degraded");
    assert_eq!(reply["result"]["links"]["refused"], true);
    assert!(reply["result"]["text"]["id"].is_string());
    assert_eq!(reply["refusal"]["code"], "too_many_link_occurrences");
    let edges = runtime
        .list_edges(
            &token,
            EdgeListFilter {
                source_id: Some(page_id),
                relations: vec![EdgeRelation::LinksTo],
                ..Default::default()
            },
            100,
            0,
        )
        .await
        .unwrap();
    assert_eq!(
        edges.len(),
        2,
        "the refused links kind does not reconcile edges"
    );
    assert_eq!(
        edges
            .iter()
            .find(|edge| edge.target_id == marked_id)
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()["web_extract"],
        true
    );
    assert!(edges
        .iter()
        .find(|edge| edge.target_id == unmarked_id)
        .unwrap()
        .metadata
        .is_none());
    let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
    let note = runtime
        .notes(&token)
        .unwrap()
        .get_note(receipt_id)
        .await
        .unwrap()
        .unwrap();
    let properties = note.properties.unwrap();
    let request = &properties["request"];
    assert_eq!(request["status"], "degraded");
    assert_eq!(request["refusal"]["code"], "too_many_link_occurrences");
    assert_eq!(request["links_complete"], false);
    assert!(request["legacy_claimed"].is_null());
    assert!(request["legacy_unclaimed"].is_null());
    let source = runtime
        .core()
        .attachments()
        .unwrap()
        .get_attachment(receipt_id, "source")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source.content_ref.to_string(), source_ref);
}

// extract(text)'s excerpt cap is a BYTE bound, and the cut never
// splits a multi-byte character even when the raw byte offset lands
// mid-character.
#[tokio::test]
async fn extract_text_caps_excerpt_at_bytes_not_chars_and_never_splits_a_character() {
    let (runtime, token, _dir) = test_runtime().await;
    // Every character below is 2 bytes (`\u{00e9}`) except a single
    // 1-byte `x` prefix, chosen so a raw cut at exactly
    // `MAX_TEXT_EXCERPT_BYTES` (200_000, even) lands in the middle of
    // a character: the prefix shifts every character's start to an
    // odd byte offset, so offset 200_000 sits inside the character at
    // byte range [199_999, 200_001) rather than on a boundary. A
    // truncation that slices without walking back to a char boundary
    // panics on this input; one that truncates by `.chars().take(N)`
    // instead of bytes would let roughly twice the intended byte
    // budget through (every char here is 2 bytes) and fail the
    // length assertion below.
    let content = format!("x{}", "\u{00e9}".repeat(150_000));
    let html = format!("<html><body><p>{content}</p></body></html>");
    let page_id = seed_page(
        &runtime,
        &token,
        "https://origin.example.test/big-multibyte",
        "text/html",
        html.as_bytes(),
    )
    .await;

    let derived = Arc::new(admit_derived_buffers(&DERIVED_ADMISSION, 0).await.unwrap());
    let text_id = extract_text(
        &runtime,
        &token,
        page_id,
        "https://origin.example.test/big-multibyte",
        blake3::hash(html.as_bytes()).to_hex().as_ref(),
        None,
        html.as_bytes(),
        &derived,
    )
    .await
    .expect("extract_text does not panic on a non-boundary byte cut");

    let entity = runtime
        .entities(&token)
        .unwrap()
        .get_entity(text_id)
        .await
        .unwrap()
        .unwrap();
    let store = runtime.require_blob_store().unwrap();
    let content_ref = khive_storage::ContentRef::from_hex(
        entity.properties.unwrap()["blob_ref"].as_str().unwrap(),
    )
    .unwrap();
    let excerpt_bytes = store
        .get_bounded_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await
        .unwrap();

    assert!(
        excerpt_bytes.len() <= MAX_TEXT_EXCERPT_BYTES,
        "excerpt must respect the byte cap even for an all-multibyte document, got {}",
        excerpt_bytes.len()
    );
    assert_eq!(
            excerpt_bytes.len(),
            199_999,
            "cut walks back exactly one byte from the mid-character offset to the nearest char boundary"
        );
    assert!(
        String::from_utf8(excerpt_bytes).is_ok(),
        "truncation must never split a multi-byte character"
    );
}

// extract on a document with no stored body refuses `not_fetched`.
#[tokio::test]
async fn extract_on_unfetched_document_refuses_not_fetched() {
    let (runtime, token, _dir) = test_runtime().await;
    let url = Url::parse("https://origin.example.test/never-fetched").unwrap();
    let canonical = identity::canonicalize(url);
    let site = identity::site_id(&khive_types::Namespace::local(), &canonical);
    let id = identity::document_id(site, &identity::path_and_query(&canonical));
    crate::entities::get_or_create(
        &runtime,
        &token,
        id,
        "document",
        "resource",
        canonical.as_ref(),
        json!({ "url": canonical.to_string(), "status": Value::Null }),
    )
    .await
    .unwrap();

    let err = run_extract(
        &runtime,
        &token,
        ExtractParams {
            id: Some(id),
            url: None,
            kinds: None,
            link_limit: None,
            namespace: None,
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not_fetched"), "{err}");
}
