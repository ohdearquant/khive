//! B1 exercises the production post-network settlement with the same empty
//! GET body a mechanical 304 yields. No HTTP/egress coverage is claimed here.
use super::*;
use crate::fetch::{settle_content, RedirectHop};
use khive_storage::{ContentRef, Direction};
use khive_types::Namespace;
use std::sync::Arc;

async fn fixture() -> (KhiveRuntime, NamespaceToken, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0).unwrap();
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
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

fn not_modified(final_url: &Url) -> HopOutcome {
    HopOutcome {
        status: 304,
        final_url: final_url.clone(),
        headers: reqwest::header::HeaderMap::new(),
        redirect_to: None,
        body: Some((Vec::new(), false)),
    }
}

// Must fail when the redirected-304 restoration is removed: placeholder
// blob_ref/status/type/root assertions fail. The same-ref control must keep
// the complete terminal row and its existing metadata unchanged.
#[tokio::test]
async fn redirected_304_restores_validated_cache_without_storing_empty_body() {
    for status in [301, 302, 307, 308] {
        for terminal_state in ["absent", "placeholder", "different", "same"] {
            let (runtime, token, _dir) = fixture().await;
            let old_url = Url::parse("https://old.example/document").unwrap();
            let final_url = Url::parse("https://final.example/document").unwrap();
            let body = b"<p>validated cached representation</p>";
            let source = settle_content(
                &runtime,
                &token,
                &old_url,
                Some("text/html; charset=utf-8"),
                200,
                Some("source-etag"),
                Some("Mon, 21 Sep 2026 12:00:00 GMT"),
                Some((body.to_vec(), false)),
            )
            .await
            .unwrap();
            let reference = source.content_ref.as_ref().unwrap();
            let final_id = document_id(&final_url);
            match terminal_state {
                "placeholder" => {
                    crate::fetch::mint_bare(&runtime, &token, &final_url)
                        .await
                        .unwrap();
                }
                "different" | "same" => {
                    let prior_body = if terminal_state == "same" {
                        body.as_slice()
                    } else {
                        b"<p>different terminal cache</p>".as_slice()
                    };
                    settle_content(
                        &runtime,
                        &token,
                        &final_url,
                        Some("text/html; charset=utf-8"),
                        200,
                        Some("terminal-etag"),
                        None,
                        Some((prior_body.to_vec(), false)),
                    )
                    .await
                    .unwrap();
                }
                _ => {}
            }
            let before_terminal = runtime
                .entities(&token)
                .unwrap()
                .get_entity(final_id)
                .await
                .unwrap();
            let before_source = runtime
                .entities(&token)
                .unwrap()
                .get_entity(source.id)
                .await
                .unwrap()
                .unwrap();
            let hop = RedirectHop {
                from: old_url.clone(),
                to: final_url.clone(),
                status,
            };
            let reply = settle_refresh(
                &runtime,
                &token,
                source.id,
                old_url.as_str(),
                reference,
                not_modified(&final_url),
                &[hop],
            )
            .await
            .unwrap();
            assert_eq!(reply["status"], 304);
            assert_eq!(reply["final_id"], final_id.to_string());
            assert_eq!(reply["changed"], terminal_state != "same");
            let terminal = runtime
                .entities(&token)
                .unwrap()
                .get_entity(final_id)
                .await
                .unwrap()
                .unwrap();
            let properties = terminal.properties.as_ref().unwrap();
            assert_eq!(properties["url"], final_url.as_str());
            assert_eq!(properties["blob_ref"], reference.as_str());
            assert_eq!(properties["content_digest"], reference.as_str());
            assert_eq!(properties["size"], body.len() as u64);
            assert_eq!(properties["status"], 200);
            assert_eq!(properties["content_type"], "text/html; charset=utf-8");
            assert!(properties["fetched_at"].as_str().is_some());
            assert_eq!(terminal.entity_type.as_deref(), Some("page"));
            if terminal_state == "same" {
                assert_eq!(
                    serde_json::to_value(&terminal).unwrap(),
                    serde_json::to_value(before_terminal.unwrap()).unwrap()
                );
                assert_eq!(properties["etag"], "terminal-etag");
            } else {
                assert_eq!(properties["etag"], "source-etag");
                assert_eq!(properties["last_modified"], "Mon, 21 Sep 2026 12:00:00 GMT");
            }
            let roots = runtime
                .core()
                .attachments()
                .unwrap()
                .list_attachments(final_id)
                .await
                .unwrap();
            assert_eq!(roots.len(), 1);
            assert_eq!(roots[0].content_ref.to_string(), *reference);
            assert_eq!(roots[0].size_bytes, Some(body.len() as u64));
            let empty_ref = ContentRef::from_hex(blake3::hash(&[]).to_hex().to_string()).unwrap();
            assert!(crate::blob_store(&runtime)
                .unwrap()
                .size(&empty_ref)
                .await
                .unwrap()
                .is_none());
            let after_source = runtime
                .entities(&token)
                .unwrap()
                .get_entity(source.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                after_source.properties.as_ref().unwrap()["url"],
                old_url.as_str()
            );
            assert_eq!(
                after_source.properties.as_ref().unwrap()["blob_ref"],
                reference.as_str()
            );
            if matches!(status, 302 | 307) {
                assert_eq!(
                    serde_json::to_value(after_source).unwrap(),
                    serde_json::to_value(before_source).unwrap()
                );
            }
            let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
            let receipt = runtime
                .notes(&token)
                .unwrap()
                .get_note(receipt_id)
                .await
                .unwrap()
                .unwrap();
            let request = &receipt.properties.as_ref().unwrap()["request"];
            assert_eq!(request["status"], 304);
            assert_eq!(request["redirect_chain"][0]["status"], status);
            assert_eq!(request["redirect_chain"][0]["to"], final_url.as_str());
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
                .iter()
                .any(|hit| hit.node_id == final_id));
        }
    }
}

// A source changed after sending its conditional headers cannot supply
// metadata for the old validated reference. Refuse before any redirect write.
#[tokio::test]
async fn redirected_304_refuses_changed_source_cache_before_graph_settlement() {
    let (runtime, token, _dir) = fixture().await;
    let old_url = Url::parse("https://source.example/document").unwrap();
    let final_url = Url::parse("https://new.example/document").unwrap();
    let original = settle_content(
        &runtime,
        &token,
        &old_url,
        Some("text/html"),
        200,
        Some("old"),
        None,
        Some((b"<p>old</p>".to_vec(), false)),
    )
    .await
    .unwrap();
    settle_content(
        &runtime,
        &token,
        &old_url,
        Some("text/html"),
        200,
        Some("new"),
        None,
        Some((b"<p>new</p>".to_vec(), false)),
    )
    .await
    .unwrap();
    let before = runtime
        .entities(&token)
        .unwrap()
        .get_entity(original.id)
        .await
        .unwrap()
        .unwrap();
    let error = settle_refresh(
        &runtime,
        &token,
        original.id,
        old_url.as_str(),
        original.content_ref.as_ref().unwrap(),
        not_modified(&final_url),
        &[RedirectHop {
            from: old_url.clone(),
            to: final_url.clone(),
            status: 301,
        }],
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("cached_body_changed"), "{error}");
    let after = runtime
        .entities(&token)
        .unwrap()
        .get_entity(original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    for id in [
        document_id(&final_url),
        crate::identity::site_id(&final_url),
    ] {
        assert!(runtime
            .entities(&token)
            .unwrap()
            .get_entity(id)
            .await
            .unwrap()
            .is_none());
    }
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .count_notes(token.namespace().as_str(), Some("observation"))
            .await
            .unwrap(),
        0
    );
}

// S1: each direct refresh read must refuse a foreign row before settlement.
// Removing an individual guard must fail its corresponding branch here.
#[tokio::test]
async fn refresh_refuses_foreign_original_terminal_and_cached_source() {
    for foreign_read in ["original", "terminal", "cached_source"] {
        let (runtime, token, _dir) = fixture().await;
        let foreign = runtime
            .authorize(Namespace::parse("foreign").unwrap())
            .unwrap();
        let old_url = Url::parse("https://source.example/document").unwrap();
        let final_url = Url::parse("https://terminal.example/document").unwrap();
        let source_token = if foreign_read == "terminal" {
            &token
        } else {
            &foreign
        };
        let source = settle_content(
            &runtime,
            source_token,
            &old_url,
            Some("text/html"),
            200,
            Some("etag"),
            None,
            Some((b"<p>private cached body</p>".to_vec(), false)),
        )
        .await
        .unwrap();
        let source_before = runtime
            .entities(source_token)
            .unwrap()
            .get_entity(source.id)
            .await
            .unwrap()
            .unwrap();
        let final_id = document_id(&final_url);
        if foreign_read == "terminal" {
            // Matching cache bypasses body settlement; only the early
            // terminal read guard can reject before receipt creation.
            settle_content(
                &runtime,
                &foreign,
                &final_url,
                Some("text/html"),
                200,
                Some("etag"),
                None,
                Some((b"<p>private cached body</p>".to_vec(), false)),
            )
            .await
            .unwrap();
        }
        let terminal_before = runtime
            .entities(&token)
            .unwrap()
            .get_entity(final_id)
            .await
            .unwrap();
        let error = if foreign_read == "original" {
            run_refresh(
                &runtime,
                &token,
                &SystemResolver,
                &runtime.config().web,
                RefreshParams {
                    id: source.id,
                    max_bytes: None,
                    timeout_s: None,
                    namespace: None,
                },
            )
            .await
            .unwrap_err()
        } else {
            settle_refresh(
                &runtime,
                &token,
                source.id,
                old_url.as_str(),
                source.content_ref.as_ref().unwrap(),
                not_modified(&final_url),
                &[RedirectHop {
                    from: old_url.clone(),
                    to: final_url.clone(),
                    status: 302,
                }],
            )
            .await
            .unwrap_err()
        };
        let refused_id = if foreign_read == "terminal" {
            final_id
        } else {
            source.id
        };
        assert!(matches!(&error, RuntimeError::Khive(_)), "{error}");
        assert_eq!(
            error.to_string(),
            format!("web entity not found: {refused_id}")
        );
        let source_after = runtime
            .entities(source_token)
            .unwrap()
            .get_entity(source.id)
            .await
            .unwrap()
            .unwrap();
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
            runtime
                .notes(&token)
                .unwrap()
                .count_notes(token.namespace().as_str(), Some("observation"))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            runtime
                .notes(&foreign)
                .unwrap()
                .count_notes(foreign.namespace().as_str(), Some("observation"))
                .await
                .unwrap(),
            0
        );
    }
}
