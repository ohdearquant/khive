//! Query bytes belong to the request address independently of D1's identity key.
//! HTTP mechanics are driven below egress, as in the existing refresh tests.
use super::*;
use crate::fetch::{run_one_hop, settle_content, RedirectHop};
use khive_types::Namespace;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn fixture() -> (KhiveRuntime, NamespaceToken, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
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

async fn capture_fetch_and_refresh() -> (u16, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let mut targets = Vec::new();
        for status in [200, 304] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut buffer = [0; 1024];
                let count = stream.read(&mut buffer).await.unwrap();
                assert!(count > 0, "request headers must complete");
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() < 16_384);
            }
            targets.push(
                String::from_utf8(request)
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap()
                    .to_owned(),
            );
            let body = if status == 200 { "fetched body" } else { "" };
            let reason = if status == 200 { "OK" } else { "Not Modified" };
            let response = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nETag: query-test\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
        targets
    });
    (port, task)
}

// Must fail if settlement stores a sorted/form-reencoded URL, or refresh rewrites it.
#[tokio::test]
async fn refresh_preserves_original_query_in_request() {
    for query in [
        "?add=1&mul=2",
        "?mul=2&add=1",
        "?a=1&b=2&a=3",
        "?a=1&a=3&b=2",
        "?id=%FF",
        "?id=%FE",
        "?q=a+b",
        "?q=a%20b",
        "?flag",
        "?flag=",
        "?a=1&a=2",
        "?a=2&a=1",
        "?sig=a%2Fb%3Dc%2B&z=2&a=1",
        "?",
        "",
    ] {
        let (runtime, token, _dir) = fixture().await;
        let (port, captured) = capture_fetch_and_refresh().await;
        let url = Url::parse(&format!("http://127.0.0.1:{port}/page{query}")).unwrap();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let fetched = run_one_hop(
            &client,
            &url,
            reqwest::Method::GET,
            &[],
            10_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
        let settled = settle_content(
            &runtime,
            &token,
            &fetched.final_url,
            Some("text/plain"),
            fetched.status,
            Some("query-test"),
            None,
            fetched.body,
        )
        .await
        .unwrap();
        let entity = runtime
            .entities(&token)
            .unwrap()
            .get_entity(settled.id)
            .await
            .unwrap()
            .unwrap();
        let properties = entity.properties.unwrap();
        assert_eq!(properties["url"], url.as_str(), "query={query}");
        let refresh_url = stored_request_url(&properties).unwrap();
        assert_eq!(refresh_url, url);
        let outcome = run_one_hop(
            &client,
            &refresh_url,
            reqwest::Method::GET,
            &[("If-None-Match".into(), "query-test".into())],
            10_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
        settle_refresh(
            &runtime,
            &token,
            settled.id,
            refresh_url.as_str(),
            settled.content_ref.as_deref().unwrap(),
            outcome,
            &[],
        )
        .await
        .unwrap();
        let targets = tokio::time::timeout(Duration::from_secs(5), captured)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(targets, vec![format!("GET /page{query} HTTP/1.1"); 2]);
        let after = runtime
            .entities(&token)
            .unwrap()
            .get_entity(settled.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.properties.unwrap()["url"], url.as_str());
    }
}

// Both a changed response and a redirected 304 must retain the terminal address.
#[tokio::test]
async fn redirected_refresh_preserves_terminal_request_query() {
    for redirect_status in [301, 302, 307, 308] {
        for response_status in [200, 304] {
            let (runtime, token, _dir) = fixture().await;
            let source_url = Url::parse("https://query.example/old?z=2&id=%FE").unwrap();
            let terminal_url = Url::parse("https://query.example/new?z=1&id=%FF&flag").unwrap();
            let source = settle_content(
                &runtime,
                &token,
                &source_url,
                Some("text/plain"),
                200,
                Some("query-test"),
                None,
                Some((b"cached body".to_vec(), false)),
            )
            .await
            .unwrap();
            let reply = settle_refresh(
                &runtime,
                &token,
                source.id,
                source_url.as_str(),
                source.content_ref.as_deref().unwrap(),
                HopOutcome {
                    status: response_status,
                    final_url: terminal_url.clone(),
                    headers: reqwest::header::HeaderMap::new(),
                    redirect_to: None,
                    body: Some((
                        if response_status == 200 {
                            b"changed body".to_vec()
                        } else {
                            vec![]
                        },
                        false,
                    )),
                },
                &[RedirectHop {
                    from: source_url.clone(),
                    to: terminal_url.clone(),
                    status: redirect_status,
                }],
            )
            .await
            .unwrap();
            let final_id = Uuid::parse_str(reply["final_id"].as_str().unwrap()).unwrap();
            for (id, expected) in [(source.id, &source_url), (final_id, &terminal_url)] {
                let entity = runtime
                    .entities(&token)
                    .unwrap()
                    .get_entity(id)
                    .await
                    .unwrap()
                    .unwrap();
                let properties = entity.properties.unwrap();
                assert_eq!(properties["url"], expected.as_str());
                assert_eq!(stored_request_url(&properties).unwrap(), *expected);
            }
        }
    }
}

#[tokio::test]
async fn unfetched_targets_preserve_request_query_for_future_fetch() {
    for producer in ["bare", "links", "sitemap", "feed"] {
        let (runtime, token, _dir) = fixture().await;
        let target = Url::parse("https://target.example/item?z=1&id=%FF#section").unwrap();
        if producer == "bare" {
            crate::fetch::mint_bare(&runtime, &token, &target)
                .await
                .unwrap();
        } else {
            let (content_type, body) = match producer {
                "links" => ("text/html", format!("<a href=\"{target}\">target</a>")),
                "sitemap" => (
                    "application/xml",
                    format!("<urlset><url><loc>{target}</loc></url></urlset>"),
                ),
                "feed" => (
                    "application/atom+xml",
                    format!("<feed><entry><link href=\"{target}\"/></entry></feed>"),
                ),
                _ => unreachable!(),
            };
            let source = settle_content(
                &runtime,
                &token,
                &Url::parse("https://source.example/").unwrap(),
                Some(content_type),
                200,
                None,
                None,
                Some((body.into_bytes(), false)),
            )
            .await
            .unwrap();
            WebPack::new(runtime.clone())
                .handle_extract(&token, json!({ "id": source.id, "kinds": [producer] }))
                .await
                .unwrap();
        }
        let canonical = crate::identity::canonicalize(target.clone());
        let id = crate::identity::document_id(
            crate::identity::site_id(&canonical),
            &crate::identity::path_and_query(&canonical),
        );
        let entity = runtime
            .entities(&token)
            .unwrap()
            .get_entity(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            entity.properties.unwrap()["url"],
            "https://target.example/item?z=1&id=%FF"
        );
    }
}
