//! A second producer consumes only the public library API. The fixture's
//! expected row and receipt fields are the `web.fetch` persisted GET shape.

use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_pack_web::identity;
use khive_pack_web::producer::{self, Capture, CaptureHeaders, SelectionHeaders};
use khive_pack_web::WebPack;
use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
use khive_storage::{AttachmentSubstrate, ContentRef, Direction, EdgeRelation};
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;

fn fixture() -> (
    KhiveRuntime,
    khive_runtime::NamespaceToken,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(WebPack::new(runtime.clone()));
    runtime.install_edge_rules(builder.build().unwrap().all_edge_rules());
    let store = khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();
    runtime.install_blob_store(Arc::new(store)).unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    (runtime, token, dir)
}

#[tokio::test]
async fn external_producer_mints_the_fetch_page_blob_edges_and_receipt_shape() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://Example.test:443/article?z=9&a=2#section").unwrap();
    let canonical = identity::canonicalize(url.clone());
    let expected_site = identity::site_id(&canonical);
    let expected_document =
        identity::document_id(expected_site, &identity::path_and_query(&canonical));
    let body = b"<html><body>fixture page</body></html>".to_vec();
    let digest = ContentRef::from_digest_bytes(blake3::hash(&body).as_bytes());

    let (site, document) = producer::mint_resource(&runtime, &token, &url)
        .await
        .unwrap();
    assert_eq!((site, document), (expected_site, expected_document));
    assert_eq!(
        runtime
            .entities(&token)
            .unwrap()
            .get_entity(document)
            .await
            .unwrap()
            .unwrap()
            .entity_type
            .as_deref(),
        Some("resource")
    );

    let mut capture = Capture::get(url.clone(), 200, body.clone());
    capture.headers = CaptureHeaders {
        content_type: Some("text/html; charset=utf-8".to_string()),
        content_length: Some(body.len().to_string()),
        etag: Some("\"fixture-v1\"".to_string()),
        vary: vec!["Accept-Language".to_string()],
        content_language: vec!["en".to_string()],
        ..Default::default()
    };
    capture.selection = SelectionHeaders {
        accept: vec!["text/html".to_string()],
        accept_language: vec!["en".to_string()],
    };
    let reply = producer::store_capture(&runtime, &token, capture)
        .await
        .unwrap();
    let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();

    assert_eq!(reply["id"], expected_document.to_string());
    assert_eq!(reply["content_ref"], digest.to_string());
    assert_eq!(reply["bytes"], body.len() as u64);
    assert_eq!(reply["truncated"], false);
    assert_eq!(reply["redirects"], 0);
    assert!(reply["body"].is_null());

    let site_row = runtime
        .entities(&token)
        .unwrap()
        .get_entity(site)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(site_row.entity_type.as_deref(), Some("site"));
    assert_eq!(
        site_row.properties,
        Some(json!({"scheme":"https", "host":"example.test", "port":443}))
    );

    let document_row = runtime
        .entities(&token)
        .unwrap()
        .get_entity(document)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(document_row.kind, "document");
    assert_eq!(document_row.entity_type.as_deref(), Some("page"));
    assert_eq!(document_row.name, canonical.as_str());
    let mut properties = document_row.properties.unwrap();
    assert!(properties["fetched_at"].is_string());
    properties.as_object_mut().unwrap().remove("fetched_at");
    assert_eq!(
        properties,
        json!({
            "url": identity::request_url(url).to_string(),
            "content_type": "text/html; charset=utf-8",
            "blob_ref": digest.to_string(),
            "content_digest": digest.to_string(),
            "size": body.len() as u64,
            "status": 200,
            "etag": "\"fixture-v1\"",
            "last_modified": null,
            "truncated": false,
            "vary": ["Accept-Language"],
            "content_language": "en",
            "request_headers": {
                "accept": ["text/html"],
                "accept-language": ["en"],
                "accept-encoding": ["identity"]
            },
            "capture_receipt_id": receipt_id.to_string()
        })
    );

    let attachment = runtime
        .core()
        .attachments()
        .unwrap()
        .get_attachment(document, "content")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(attachment.substrate, AttachmentSubstrate::Entity);
    assert_eq!(attachment.content_ref, digest);
    assert_eq!(
        runtime
            .blob_store()
            .unwrap()
            .get_bounded_verified(&digest, body.len() as u64)
            .await
            .unwrap(),
        body
    );

    let contained = runtime
        .neighbors(
            &token,
            site,
            Direction::Out,
            None,
            Some(vec![EdgeRelation::Contains]),
        )
        .await
        .unwrap();
    assert_eq!(contained.len(), 1);
    assert_eq!(contained[0].node_id, document);
    let annotated = runtime
        .neighbors(
            &token,
            receipt_id,
            Direction::Out,
            None,
            Some(vec![EdgeRelation::Annotates]),
        )
        .await
        .unwrap();
    assert_eq!(annotated.len(), 1);
    assert_eq!(annotated[0].node_id, document);

    let receipt = runtime
        .notes(&token)
        .unwrap()
        .get_note(receipt_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.kind, "observation");
    assert_eq!(
        receipt.content,
        "web.fetch GET https://example.test/article?z=9&a=2#section"
    );
    let receipt_properties = receipt.properties.unwrap();
    assert_eq!(receipt_properties["tags"], json!(["web.receipt"]));
    let mut request: Value = receipt_properties["request"].clone();
    assert!(request["fetched_at"].is_string());
    request.as_object_mut().unwrap().remove("fetched_at");
    assert_eq!(
        request,
        json!({
            "verb": "web.fetch",
            "method": "GET",
            "final_url": "https://example.test/article?z=9&a=2#section",
            "status": 200,
            "headers": {
                "content-type": "text/html; charset=utf-8",
                "content-length": body.len().to_string(),
                "etag": "\"fixture-v1\"",
                "vary": ["Accept-Language"],
                "content-language": "en"
            },
            "request_headers": {
                "accept": ["text/html"],
                "accept-language": ["en"],
                "accept-encoding": ["identity"]
            },
            "bytes": body.len() as u64,
            "truncated": false,
            "content_ref": digest.to_string(),
            "content_digest": digest.to_string(),
            "size": body.len() as u64,
            "body_entity_id": document.to_string(),
            "redirects": 0,
            "redirect_chain": []
        })
    );
}

#[tokio::test]
async fn declared_compressed_body_is_refused_before_minting() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://example.test/compressed").unwrap();
    let mut capture = Capture::get(url.clone(), 200, b"encoded".to_vec());
    capture.headers.content_encoding = vec!["gzip".to_string()];
    let error = producer::store_capture(&runtime, &token, capture)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unsupported_content_encoding"));
    let canonical = identity::canonicalize(url);
    let site = identity::site_id(&canonical);
    assert!(runtime
        .entities(&token)
        .unwrap()
        .get_entity(site)
        .await
        .unwrap()
        .is_none());
}

async fn document_properties(
    runtime: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    document: Uuid,
) -> Option<Value> {
    runtime
        .entities(token)
        .unwrap()
        .get_entity(document)
        .await
        .unwrap()
        .and_then(|row| row.properties)
}

#[tokio::test]
async fn not_modified_capture_is_refused_and_keeps_the_stored_body() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://example.test/cached").unwrap();
    let body = b"<html><body>cached page</body></html>".to_vec();
    let mut capture = Capture::get(url.clone(), 200, body);
    capture.headers.content_type = Some("text/html".to_string());
    let reply = producer::store_capture(&runtime, &token, capture)
        .await
        .unwrap();
    let document = Uuid::parse_str(reply["id"].as_str().unwrap()).unwrap();
    let stored = document_properties(&runtime, &token, document).await;
    assert!(stored.as_ref().is_some_and(|p| p["status"] == 200));

    let mut not_modified = Capture::get(url, 304, Vec::new());
    not_modified.headers.content_type = Some("text/html".to_string());
    let error = producer::store_capture(&runtime, &token, not_modified)
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("a 304 capture carries no representation"));
    assert_eq!(
        document_properties(&runtime, &token, document).await,
        stored
    );
}

#[tokio::test]
async fn bodied_no_content_capture_is_refused_before_minting() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://example.test/no-content").unwrap();
    let mut capture = Capture::get(url.clone(), 204, b"encoded".to_vec());
    capture.headers.content_type = Some("text/html".to_string());
    capture.headers.content_encoding = vec!["gzip".to_string()];
    let error = producer::store_capture(&runtime, &token, capture)
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("a 204 capture cannot carry body bytes"));
    let site = identity::site_id(&identity::canonicalize(url));
    assert!(runtime
        .entities(&token)
        .unwrap()
        .get_entity(site)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn empty_no_content_capture_skips_the_coding_check_like_fetch() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://example.test/empty").unwrap();
    let mut capture = Capture::get(url, 204, Vec::new());
    capture.headers.content_encoding = vec!["gzip".to_string()];
    let reply = producer::store_capture(&runtime, &token, capture)
        .await
        .unwrap();
    assert_eq!(reply["bytes"], 0);
}

#[tokio::test]
async fn interim_status_capture_is_refused() {
    let (runtime, token, _dir) = fixture();
    let url = Url::parse("https://example.test/interim").unwrap();
    for capture in [
        Capture::get(url.clone(), 101, Vec::new()),
        Capture::head(url.clone(), 100),
    ] {
        let error = producer::store_capture(&runtime, &token, capture)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not an interim 1xx"));
    }
}
