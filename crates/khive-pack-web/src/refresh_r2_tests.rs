//! B1 exercises the production post-network settlement with the same empty
//! GET body a mechanical 304 yields. No HTTP/egress coverage is claimed here.
use super::*;
use crate::fetch::{settle_content, RedirectHop};
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

// A 304 from a redirect target cannot validate the source address's body.
// The terminal row, whether absent or already fetched, must remain unchanged.
#[tokio::test]
async fn redirected_304_never_copies_the_source_representation() {
    for status in [301, 302, 307, 308] {
        for terminal_state in ["absent", "different"] {
            let (runtime, token, _dir) = fixture().await;
            let old_url = Url::parse("https://old.example/document").unwrap();
            let final_url = Url::parse("https://final.example/document").unwrap();
            let source = settle_content(
                &runtime,
                &token,
                &old_url,
                Some("text/html"),
                200,
                Some("source-etag"),
                None,
                Some((b"source body".to_vec(), false)),
            )
            .await
            .unwrap();
            let final_id = document_id(&final_url);
            if terminal_state == "different" {
                settle_content(
                    &runtime,
                    &token,
                    &final_url,
                    Some("text/html"),
                    200,
                    Some("terminal-etag"),
                    None,
                    Some((b"terminal body".to_vec(), false)),
                )
                .await
                .unwrap();
            }
            let source_before = runtime
                .entities(&token)
                .unwrap()
                .get_entity(source.id)
                .await
                .unwrap()
                .unwrap();
            let terminal_before = runtime
                .entities(&token)
                .unwrap()
                .get_entity(final_id)
                .await
                .unwrap();
            let error = settle_refresh(
                &runtime,
                &token,
                source.id,
                old_url.as_str(),
                source.content_ref.as_ref().unwrap(),
                not_modified(&final_url),
                &[RedirectHop {
                    from: old_url.clone(),
                    to: final_url.clone(),
                    status,
                }],
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains("redirected_not_modified"),
                "{error}"
            );
            let source_after = runtime
                .entities(&token)
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
        }
    }
}

#[tokio::test]
async fn redirected_304_is_refused_even_when_target_has_same_document_identity() {
    let (runtime, token, _dir) = fixture().await;
    let source_url = Url::parse("https://example.test/document").unwrap();
    let fragment_url = Url::parse("https://example.test/document#section").unwrap();
    assert_eq!(document_id(&source_url), document_id(&fragment_url));
    let source = settle_content(
        &runtime,
        &token,
        &source_url,
        Some("text/html"),
        200,
        Some("source-etag"),
        None,
        Some((b"source body".to_vec(), false)),
    )
    .await
    .unwrap();
    let error = settle_refresh(
        &runtime,
        &token,
        source.id,
        source_url.as_str(),
        source.content_ref.as_ref().unwrap(),
        not_modified(&fragment_url),
        &[RedirectHop {
            from: source_url.clone(),
            to: fragment_url,
            status: 302,
        }],
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("redirected_not_modified"),
        "{error}"
    );
}

// A redirected 304 remains invalid even if the source changed meanwhile.
#[tokio::test]
async fn redirected_304_refuses_before_graph_settlement_after_source_changes() {
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
    assert!(
        error.to_string().contains("redirected_not_modified"),
        "{error}"
    );
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

// S1: the original and redirect target rows are namespace-scoped reads.
#[tokio::test]
async fn refresh_refuses_foreign_original_and_terminal() {
    for foreign_read in ["original", "terminal"] {
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
            // The terminal read guard runs before body settlement.
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
                HopOutcome {
                    status: 200,
                    final_url: final_url.clone(),
                    headers: reqwest::header::HeaderMap::new(),
                    redirect_to: None,
                    body: Some((b"fresh terminal body".to_vec(), false)),
                },
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
