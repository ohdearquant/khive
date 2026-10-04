//! B1 exercises the production post-network settlement with the same empty
//! GET body a mechanical 304 yields. No HTTP/egress coverage is claimed here.
use super::*;
use crate::fetch::{settle_content, RedirectHop};
use khive_storage::ContentRef;
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
        crate::identity::site_id(&khive_types::Namespace::local(), &canonical),
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
        crate::identity::site_id(&khive_types::Namespace::local(), &final_url),
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

// Deliberate foreign attribution at a caller-derived ID must still refuse.
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
            let mut terminal = Entity::new("foreign", "document", "private terminal")
                .with_entity_type(Some("page"))
                .with_properties(json!({"url": final_url.as_str(), "marker": "retain"}));
            terminal.id = final_id;
            assert!(runtime
                .entities(&foreign)
                .unwrap()
                .insert_entity_if_absent(terminal)
                .await
                .unwrap());
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
                &egress::PinnedClients::default(),
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

#[tokio::test]
async fn local_refresh_preserves_foreign_terminal_at_its_own_namespaced_id() {
    let (runtime, token, _dir) = fixture().await;
    let foreign = runtime
        .authorize(Namespace::parse("foreign").unwrap())
        .unwrap();
    let old_url = Url::parse("https://source.example/document").unwrap();
    let final_url = Url::parse("https://terminal.example/document").unwrap();
    let source = settle_content(
        &runtime,
        &token,
        &old_url,
        Some("text/html"),
        200,
        Some("local-etag"),
        None,
        Some((b"local source body".to_vec(), false)),
    )
    .await
    .unwrap();
    let foreign_terminal = settle_content(
        &runtime,
        &foreign,
        &final_url,
        Some("text/html"),
        200,
        Some("foreign-etag"),
        None,
        Some((b"foreign terminal body".to_vec(), false)),
    )
    .await
    .unwrap();
    let store = runtime.entities(&token).unwrap();
    let foreign_before = store
        .get_entity(foreign_terminal.id)
        .await
        .unwrap()
        .unwrap();
    let local_terminal_id = document_id(&final_url);
    assert_ne!(local_terminal_id, foreign_terminal.id);
    assert!(store.get_entity(local_terminal_id).await.unwrap().is_none());
    let body = b"local terminal body".to_vec();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("content-type", "text/html".parse().unwrap());
    let reply = settle_refresh(
        &runtime,
        &token,
        source.id,
        old_url.as_str(),
        source.content_ref.as_ref().unwrap(),
        HopOutcome {
            status: 200,
            final_url: final_url.clone(),
            headers,
            redirect_to: None,
            body: Some((body.clone(), false)),
        },
        &[RedirectHop {
            from: old_url.clone(),
            to: final_url.clone(),
            status: 302,
        }],
    )
    .await
    .unwrap();
    assert_eq!(reply["final_id"], local_terminal_id.to_string());
    assert_eq!(reply["changed"], true);
    let local_terminal = store.get_entity(local_terminal_id).await.unwrap().unwrap();
    assert_eq!(local_terminal.namespace, "local");
    assert_eq!(
        local_terminal.properties.as_ref().unwrap()["blob_ref"],
        ContentRef::from_digest_bytes(blake3::hash(&body).as_bytes()).to_string()
    );
    assert_eq!(
        serde_json::to_value(
            store
                .get_entity(foreign_terminal.id)
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(foreign_before).unwrap()
    );
}

#[tokio::test]
async fn refresh_refuses_foreign_terminal_before_issuing_its_request() {
    use crate::egress::resolver_fixture::ScriptedResolver;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let (runtime, token, _dir) = fixture().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let source_url = Url::parse(&format!(
        "http://refresh-namespace.example.test:{port}/source"
    ))
    .unwrap();
    let terminal_url = Url::parse(&format!(
        "http://refresh-namespace.example.test:{port}/terminal"
    ))
    .unwrap();
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
    let terminal_id = document_id(&terminal_url);
    let mut terminal = Entity::new("foreign", "document", "private terminal")
        .with_entity_type(Some("page"))
        .with_properties(json!({"url": terminal_url.as_str(), "marker": "retain"}));
    terminal.id = terminal_id;
    let store = runtime.entities(&token).unwrap();
    assert!(store.insert_entity_if_absent(terminal).await.unwrap());
    let source_before = store.get_entity(source.id).await.unwrap().unwrap();
    let terminal_before = store.get_entity(terminal_id).await.unwrap().unwrap();
    let terminal_requests = Arc::new(AtomicUsize::new(0));
    let server_requests = terminal_requests.clone();
    let server_terminal_url = terminal_url.clone();
    let server = tokio::spawn(async move {
        for path in ["/source", "/terminal"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
                assert!(request.len() < 16_384);
            }
            let request = String::from_utf8(request).unwrap();
            assert_eq!(
                request.lines().next().unwrap(),
                format!("GET {path} HTTP/1.1")
            );
            let response = if path == "/source" {
                format!("HTTP/1.1 302 Found\r\nLocation: {server_terminal_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            } else {
                server_requests.fetch_add(1, Ordering::SeqCst);
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            };
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });

    // Keep public-address policy checks; only the test transport dials localhost.
    let resolver = ScriptedResolver::new(None);
    let clients = egress::PinnedClients::default();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(
            "refresh-namespace.example.test",
            std::net::SocketAddr::new("127.0.0.1".parse().unwrap(), port),
        )
        .build()
        .unwrap();
    clients.insert_for_test(&source_url, resolver.address, client);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_refresh(
            &runtime,
            &token,
            &resolver,
            &Default::default(),
            RefreshParams {
                id: source.id,
                max_bytes: None,
                timeout_s: Some(3),
                namespace: None,
            },
            &clients,
        ),
    )
    .await;
    server.abort();
    let _ = server.await;
    let error = result
        .expect("refresh exceeded the test deadline")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("web entity not found: {terminal_id}")
    );
    assert_eq!(terminal_requests.load(Ordering::SeqCst), 0);
    for (id, before) in [(source.id, source_before), (terminal_id, terminal_before)] {
        assert_eq!(
            serde_json::to_value(store.get_entity(id).await.unwrap().unwrap()).unwrap(),
            serde_json::to_value(before).unwrap()
        );
    }
}

#[tokio::test]
async fn refresh_redirects_settle_only_the_authorized_namespaces_terminal_row() {
    for status in [301, 302] {
        let (runtime, _local, _dir) = fixture().await;
        let old_url = Url::parse("https://source.example.test/page").unwrap();
        let final_url = Url::parse("https://terminal.example.test/page?z=9&a=2").unwrap();
        let alpha = runtime
            .authorize(Namespace::parse("alpha").unwrap())
            .unwrap();
        let beta = runtime
            .authorize(Namespace::parse("beta").unwrap())
            .unwrap();
        let mut alpha_ids = Vec::new();
        for url in [&old_url, &final_url] {
            let stored = settle_content(
                &runtime,
                &alpha,
                url,
                Some("text/html"),
                200,
                Some("alpha-validator"),
                None,
                Some((b"alpha body".to_vec(), false)),
            )
            .await
            .unwrap();
            alpha_ids.push(stored.id);
        }
        let alpha_store = runtime.entities(&alpha).unwrap();
        let mut alpha_before = Vec::new();
        for id in &alpha_ids {
            alpha_before.push(
                serde_json::to_value(alpha_store.get_entity(*id).await.unwrap().unwrap()).unwrap(),
            );
        }
        let source = settle_content(
            &runtime,
            &beta,
            &old_url,
            Some("text/html"),
            200,
            Some("beta-source-validator"),
            None,
            Some((b"beta source body".to_vec(), false)),
        )
        .await
        .unwrap();
        let terminal = settle_content(
            &runtime,
            &beta,
            &final_url,
            Some("text/html"),
            200,
            Some("beta-terminal-validator"),
            None,
            Some((b"beta terminal body".to_vec(), false)),
        )
        .await
        .unwrap();
        let beta_store = runtime.entities(&beta).unwrap();
        let source_before = beta_store.get_entity(source.id).await.unwrap().unwrap();
        let canonical = crate::identity::canonicalize(final_url.clone());
        let expected_terminal = crate::identity::document_id(
            crate::identity::site_id(beta.namespace(), &canonical),
            &crate::identity::path_and_query(&canonical),
        );
        assert_eq!(terminal.id, expected_terminal);
        assert_ne!(terminal.id, alpha_ids[1]);
        let new_body = b"refreshed beta body".to_vec();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "content-type",
            reqwest::header::HeaderValue::from_static("text/html"),
        );
        let reply = settle_refresh(
            &runtime,
            &beta,
            source.id,
            old_url.as_str(),
            source.content_ref.as_deref().unwrap(),
            HopOutcome {
                status: 200,
                final_url: final_url.clone(),
                headers,
                redirect_to: None,
                body: Some((new_body.clone(), false)),
            },
            &[RedirectHop {
                from: old_url.clone(),
                to: final_url.clone(),
                status,
            }],
        )
        .await
        .unwrap();
        assert_eq!(reply["final_id"], expected_terminal.to_string());
        let terminal_after = beta_store
            .get_entity(expected_terminal)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(terminal_after.namespace, "beta");
        assert_eq!(
            terminal_after.properties.as_ref().unwrap()["blob_ref"],
            khive_storage::ContentRef::from_digest_bytes(blake3::hash(&new_body).as_bytes())
                .to_string()
        );
        let source_after = beta_store.get_entity(source.id).await.unwrap().unwrap();
        assert_eq!(
            source_after.properties.as_ref().unwrap()["blob_ref"],
            source_before.properties.as_ref().unwrap()["blob_ref"]
        );
        assert_eq!(
            source_after.properties.as_ref().unwrap()["url"],
            old_url.as_str()
        );
        for (id, before) in alpha_ids.iter().zip(alpha_before) {
            assert_eq!(
                serde_json::to_value(alpha_store.get_entity(*id).await.unwrap().unwrap()).unwrap(),
                before
            );
        }
        let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            runtime
                .notes(&beta)
                .unwrap()
                .get_note(receipt_id)
                .await
                .unwrap()
                .unwrap()
                .namespace,
            "beta"
        );
    }
}
