//! Settlement regressions use the production post-network seam. They do not
//! substitute for the existing DNS, egress, or HTTP mechanics tests.
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
