use super::*;
use async_trait::async_trait;
use khive_runtime::{Namespace, RuntimeConfig};
use khive_storage::{BlobStore, ContentRef, StorageResult};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Debug)]
struct CountingStore {
    inner: khive_db::stores::blob::FsBlobStore,
    puts: AtomicUsize,
    last_buffer: AtomicUsize,
}

#[async_trait]
impl BlobStore for CountingStore {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.last_buffer
            .store(bytes.as_ptr() as usize, Ordering::SeqCst);
        self.inner.put(bytes).await
    }

    async fn get_bounded_verified(
        &self,
        reference: &ContentRef,
        max: u64,
    ) -> StorageResult<Vec<u8>> {
        self.inner.get_bounded_verified(reference, max).await
    }

    async fn exists(&self, reference: &ContentRef) -> StorageResult<bool> {
        self.inner.exists(reference).await
    }

    async fn size(&self, reference: &ContentRef) -> StorageResult<Option<u64>> {
        self.inner.size(reference).await
    }

    async fn delete(&self, reference: &ContentRef) -> StorageResult<bool> {
        self.inner.delete(reference).await
    }
}

fn fixture() -> (
    KhiveRuntime,
    NamespaceToken,
    Arc<CountingStore>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(CountingStore {
        inner: khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0).unwrap(),
        puts: AtomicUsize::new(0),
        last_buffer: AtomicUsize::new(0),
    });
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some("web-refresh-reuse-test".into()),
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(WebPack::new(runtime.clone()));
    runtime.install_edge_rules(builder.build().unwrap().all_edge_rules());
    runtime.install_blob_store(store.clone()).unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    (runtime, token, store, dir)
}

async fn seed(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    url: &Url,
    body: &[u8],
) -> crate::fetch::SettledContent {
    crate::fetch::settle_content(
        runtime,
        token,
        url,
        Some("text/html"),
        200,
        Some("old-etag"),
        None,
        Some((body.to_vec(), false)),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn refresh_moves_body_once_and_roots_the_same_reference() {
    for redirect in [None, Some(301), Some(302), Some(307), Some(308)] {
        for unchanged in [false, true] {
            let (runtime, token, store, _dir) = fixture();
            let old_url = Url::parse("https://reuse.example.test/old").unwrap();
            let final_url = if redirect.is_some() {
                Url::parse("https://reuse.example.test/new").unwrap()
            } else {
                old_url.clone()
            };
            let body = b"received representation";
            let source_body = if unchanged && redirect.is_none() {
                body.as_slice()
            } else {
                b"old representation"
            };
            let source = seed(&runtime, &token, &old_url, source_body).await;
            if unchanged && redirect.is_some() {
                seed(&runtime, &token, &final_url, body).await;
            }
            let hops: Vec<_> = redirect
                .into_iter()
                .map(|status| crate::fetch::RedirectHop {
                    from: old_url.clone(),
                    to: final_url.clone(),
                    status,
                })
                .collect();
            let received = body.to_vec();
            let received_pointer = received.as_ptr() as usize;
            store.puts.store(0, Ordering::SeqCst);
            let reply = settle_refresh(
                &runtime,
                &token,
                source.id,
                old_url.as_str(),
                source.content_ref.as_deref().unwrap(),
                HopOutcome {
                    status: 200,
                    final_url: final_url.clone(),
                    headers: reqwest::header::HeaderMap::new(),
                    redirect_to: None,
                    body: Some((received, false)),
                },
                &hops,
            )
            .await
            .unwrap();
            assert_eq!(
                store.puts.load(Ordering::SeqCst),
                1,
                "redirect={redirect:?}, unchanged={unchanged}"
            );
            assert_eq!(
                store.last_buffer.load(Ordering::SeqCst),
                received_pointer,
                "put takes ownership of the received allocation without a body clone"
            );
            assert_eq!(reply["changed"], !unchanged);
            let final_id = Uuid::parse_str(reply["final_id"].as_str().unwrap()).unwrap();
            let entity = runtime
                .entities(&token)
                .unwrap()
                .get_entity(final_id)
                .await
                .unwrap()
                .unwrap();
            let properties = entity.properties.unwrap();
            let reference = ContentRef::from_hex(properties["blob_ref"].as_str().unwrap()).unwrap();
            assert_eq!(properties["url"], final_url.as_str());
            assert_eq!(properties["size"], body.len() as u64);
            assert_eq!(
                store.get_bounded_verified(&reference, 1024).await.unwrap(),
                body
            );
            let receipt = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
            for record in [final_id, receipt] {
                let roots = runtime
                    .attachments()
                    .unwrap()
                    .list_attachments(record)
                    .await
                    .unwrap();
                assert_eq!(roots.len(), 1);
                assert_eq!(roots[0].content_ref, reference);
                assert_eq!(
                    roots[0].size_bytes,
                    Some(body.len() as u64),
                    "received length survives moving body"
                );
            }
        }
    }
}

#[tokio::test]
async fn refresh_and_fetch_share_representation_metadata() {
    for (content_type, expected_type) in [
        (Some("Text/HTML; charset=utf-8"), "page"),
        (Some("APPLICATION/XHTML+XML"), "page"),
        (Some("application/json"), "resource"),
        (None, "resource"),
    ] {
        for redirected in [false, true] {
            let (runtime, token, _store, _dir) = fixture();
            let source_url = Url::parse("https://metadata.example.test/old").unwrap();
            let final_url = if redirected {
                Url::parse("https://metadata.example.test/new").unwrap()
            } else {
                source_url.clone()
            };
            let source = seed(&runtime, &token, &source_url, b"old body").await;
            let fetched_url = Url::parse("https://metadata.example.test/fetched").unwrap();
            let body = b"new representation";
            let etag = "new-etag";
            let modified = "Mon, 21 Sep 2026 12:00:00 GMT";
            let fetched = crate::fetch::settle_content(
                &runtime,
                &token,
                &fetched_url,
                content_type,
                200,
                Some(etag),
                Some(modified),
                Some((body.to_vec(), false)),
            )
            .await
            .unwrap();
            let mut headers = reqwest::header::HeaderMap::new();
            if let Some(value) = content_type {
                headers.insert("content-type", value.parse().unwrap());
            }
            headers.insert("etag", etag.parse().unwrap());
            headers.insert("last-modified", modified.parse().unwrap());
            let hops = if redirected {
                vec![crate::fetch::RedirectHop {
                    from: source_url.clone(),
                    to: final_url.clone(),
                    status: 302,
                }]
            } else {
                vec![]
            };
            let reply = settle_refresh(
                &runtime,
                &token,
                source.id,
                source_url.as_str(),
                source.content_ref.as_deref().unwrap(),
                HopOutcome {
                    status: 200,
                    final_url,
                    headers,
                    redirect_to: None,
                    body: Some((body.to_vec(), false)),
                },
                &hops,
            )
            .await
            .unwrap();
            let final_id = Uuid::parse_str(reply["final_id"].as_str().unwrap()).unwrap();
            let entities = runtime.entities(&token).unwrap();
            let refreshed = entities.get_entity(final_id).await.unwrap().unwrap();
            let fetched = entities.get_entity(fetched.id).await.unwrap().unwrap();
            assert_eq!(refreshed.entity_type.as_deref(), Some(expected_type));
            assert_eq!(refreshed.entity_type, fetched.entity_type);
            let refreshed = refreshed.properties.unwrap();
            let fetched = fetched.properties.unwrap();
            for key in [
                "content_type",
                "blob_ref",
                "content_digest",
                "size",
                "status",
                "etag",
                "last_modified",
            ] {
                assert_eq!(
                    refreshed[key], fetched[key],
                    "metadata field {key}, redirected={redirected}"
                );
            }
            assert!(refreshed["fetched_at"].as_str().is_some());
        }
    }
}

#[tokio::test]
async fn not_modified_refresh_never_puts_cached_bytes_again() {
    for redirected in [false, true] {
        let (runtime, token, store, _dir) = fixture();
        let old_url = Url::parse("https://cache.example.test/old").unwrap();
        let final_url = if redirected {
            Url::parse("https://cache.example.test/new").unwrap()
        } else {
            old_url.clone()
        };
        let source = seed(&runtime, &token, &old_url, b"cached body").await;
        let hops = if redirected {
            vec![crate::fetch::RedirectHop {
                from: old_url.clone(),
                to: final_url.clone(),
                status: 301,
            }]
        } else {
            vec![]
        };
        store.puts.store(0, Ordering::SeqCst);
        let result = settle_refresh(
            &runtime,
            &token,
            source.id,
            old_url.as_str(),
            source.content_ref.as_deref().unwrap(),
            HopOutcome {
                status: 304,
                final_url,
                headers: reqwest::header::HeaderMap::new(),
                redirect_to: None,
                body: None,
            },
            &hops,
        )
        .await;
        assert_eq!(store.puts.load(Ordering::SeqCst), 0);
        if redirected {
            let error = result.unwrap_err();
            assert!(
                error.to_string().contains("redirected_not_modified"),
                "{error}"
            );
            continue;
        }
        let reply = result.unwrap();
        let final_id = Uuid::parse_str(reply["final_id"].as_str().unwrap()).unwrap();
        let final_entity = runtime
            .entities(&token)
            .unwrap()
            .get_entity(final_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            final_entity.properties.unwrap()["blob_ref"],
            source.content_ref.unwrap()
        );
    }
}
