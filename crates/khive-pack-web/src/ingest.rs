//! `web.ingest(source, origin?, depth?, limit?)` (ADR-191 D3).
//!
//! `source` is one of: a single URL string, a JSON array of URL strings, or
//! (when `origin` is given) a filesystem directory path laid out as
//! `origin` would serve it — `origin` then supplies the `site` identity for
//! every file in the tree (D3: "a directory laid out as an origin serves
//! it; `origin` is then required and supplies the `site` identity").
//!
//! This module is orchestration only: the URL path calls this pack's own
//! `web.fetch`/`web.extract` verb handlers exactly as an external caller
//! looping `fetch` then `extract` would (D3's own framing: "as fetch +
//! extract"), so there is no second entity-minting code path to keep in
//! sync for that arm. The disk path (A5) cannot call `handle_fetch` — there
//! is no HTTP request — so it mints entities directly via the same
//! `entities::get_or_create`/`patch` + blob-put + receipt sequence
//! `fetch::settle` uses for its terminal hop; flagged as a partial
//! duplication (not a shared code path) in LEG_B_REPORT.md.

use std::collections::VecDeque;
use std::path::Path;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::EdgeRelation;
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;

use crate::egress::Refusal;
use crate::identity;
use crate::receipt::write_receipt;
use crate::WebPack;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IngestParams {
    source: Value,
    #[serde(default)]
    origin: Option<String>,
    #[serde(default)]
    depth: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    namespace: Option<String>,
}

const DEFAULT_INGEST_LIMIT: u32 = 100;

fn guess_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
    {
        "html" | "htm" => "text/html",
        "xml" => "application/xml",
        "txt" => "text/plain",
        "json" => "application/json",
        "css" => "text/css",
        "js" => "application/javascript",
        _ => "application/octet-stream",
    }
}

/// Mint/update the entity for one disk-tree file, exactly as
/// `fetch::settle` would for the equivalent HTTP response, minus redirect
/// handling (files on disk do not redirect).
async fn ingest_disk_file(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    origin: &Url,
    site_id: Uuid,
    relative_path: &str,
    bytes: Vec<u8>,
) -> Result<Uuid, RuntimeError> {
    let target_url = origin.join(relative_path).map_err(|error| {
        RuntimeError::Internal(format!("bad relative path {relative_path:?}: {error}"))
    })?;
    let canonical = identity::canonicalize(target_url);
    let path_and_query = identity::path_and_query(&canonical);
    let id = identity::document_id(site_id, &path_and_query);
    let content_type = guess_content_type(Path::new(relative_path));
    let entity_type = if content_type == "text/html" {
        "page"
    } else {
        "resource"
    };

    let store = crate::blob_store(runtime)?;
    let content_ref = store.put(bytes.clone()).await.map_err(RuntimeError::from)?;

    crate::entities::get_or_create(
        runtime,
        token,
        id,
        "document",
        entity_type,
        canonical.as_ref(),
        json!({ "url": canonical.to_string() }),
    )
    .await?;
    crate::entities::patch(
        runtime,
        token,
        id,
        Some(entity_type),
        json!({
            "url": canonical.to_string(),
            "content_type": content_type,
            "blob_ref": content_ref.to_string(),
            "content_digest": content_ref.to_string(),
            "size": bytes.len() as u64,
            "status": 200,
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .await?;
    runtime
        .link(token, site_id, id, EdgeRelation::Contains, 1.0, None)
        .await?;

    let request_record = json!({
        "verb": "web.ingest",
        "mode": "disk",
        "url": canonical.to_string(),
        "bytes": bytes.len() as u64,
    });
    write_receipt(
        runtime,
        token,
        &format!("web.ingest (disk) {canonical}"),
        request_record,
        vec![id],
    )
    .await?;
    Ok(id)
}

fn walk_files(root: &Path) -> Result<Vec<std::path::PathBuf>, RuntimeError> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "web.ingest: cannot read {}: {error}",
                dir.display()
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| RuntimeError::Internal(error.to_string()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

async fn ingest_disk(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    root: &str,
    origin: &str,
    limit: u32,
) -> Result<Value, RuntimeError> {
    let origin_url = Url::parse(origin)
        .map_err(|error| RuntimeError::InvalidInput(format!("invalid origin: {error}")))?;
    let canonical_origin = identity::canonicalize(origin_url.clone());
    let site_id = crate::fetch::mint_bare(runtime, token, &canonical_origin)
        .await?
        .0;

    let root_path = Path::new(root);
    if !root_path.is_dir() {
        return Err(RuntimeError::InvalidInput(format!(
            "web.ingest: {root:?} is not a directory"
        )));
    }
    let files = walk_files(root_path)?;
    let mut minted = Vec::new();
    for path in files.into_iter().take(limit as usize) {
        let relative = path
            .strip_prefix(root_path)
            .map_err(|error| RuntimeError::Internal(error.to_string()))?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let bytes = std::fs::read(&path).map_err(|error| {
            RuntimeError::Internal(format!(
                "web.ingest: cannot read {}: {error}",
                path.display()
            ))
        })?;
        let id =
            ingest_disk_file(runtime, token, &canonical_origin, site_id, &relative, bytes).await?;
        minted.push(id.to_string());
    }
    Ok(json!({ "mode": "disk", "site": site_id.to_string(), "ingested": minted }))
}

async fn ingest_urls(
    pack: &WebPack,
    token: &NamespaceToken,
    urls: Vec<String>,
    depth: u32,
    limit: u32,
) -> Result<Value, RuntimeError> {
    let mut queue: VecDeque<(String, u32)> = urls.into_iter().map(|u| (u, 0)).collect();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ingested = Vec::new();

    while let Some((url_str, level)) = queue.pop_front() {
        if ingested.len() >= limit as usize {
            break;
        }
        if !visited.insert(url_str.clone()) {
            continue;
        }
        let fetch_reply = pack
            .handle_fetch(token, json!({ "url": url_str, "persist": true }))
            .await;
        let Ok(fetch_reply) = fetch_reply else {
            continue;
        };
        let Some(id_str) = fetch_reply["id"].as_str() else {
            continue;
        };
        ingested.push(id_str.to_string());

        let extract_reply = pack
            .handle_extract(token, json!({ "id": id_str, "kinds": ["links"] }))
            .await;
        if level < depth {
            if let Ok(extract_reply) = extract_reply {
                let _ = extract_reply;
                let id = Uuid::parse_str(id_str).unwrap_or_default();
                if let Ok(neighbors) = pack
                    .runtime
                    .neighbors(
                        token,
                        id,
                        khive_storage::Direction::Out,
                        None,
                        Some(vec![EdgeRelation::LinksTo]),
                    )
                    .await
                {
                    for n in neighbors {
                        if let Ok(Some(entity)) = pack
                            .runtime
                            .entities(token)
                            .unwrap()
                            .get_entity(n.node_id)
                            .await
                        {
                            if let Some(url) = entity.properties.and_then(|p| {
                                p.get("url").and_then(|v| v.as_str().map(str::to_string))
                            }) {
                                queue.push_back((url, level + 1));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(json!({ "mode": "urls", "ingested": ingested }))
}

async fn run_ingest(
    pack: &WebPack,
    token: &NamespaceToken,
    params: IngestParams,
) -> Result<Value, RuntimeError> {
    let depth = params.depth.unwrap_or(0);
    let limit = params.limit.unwrap_or(DEFAULT_INGEST_LIMIT);

    if let Some(origin) = &params.origin {
        let root = params.source.as_str().ok_or_else(|| {
            RuntimeError::InvalidInput(
                "web.ingest: source must be a directory path string when origin is given"
                    .to_string(),
            )
        })?;
        return ingest_disk(&pack.runtime, token, root, origin, limit).await;
    }

    let urls: Vec<String> = match &params.source {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    RuntimeError::from(Refusal::new(
                        "invalid_source",
                        "web.ingest: source array must contain only URL strings",
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(RuntimeError::InvalidInput(
                "web.ingest: source must be a URL string or an array of URL strings (or a directory path with origin)".to_string(),
            ))
        }
    };
    ingest_urls(pack, token, urls, depth, limit).await
}

impl WebPack {
    pub(crate) async fn handle_ingest(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let params: IngestParams = serde_json::from_value(params).map_err(|error| {
            RuntimeError::InvalidInput(format!("invalid web.ingest arguments: {error}"))
        })?;
        let effective_token =
            crate::fetch::resolve_effective_token(token, params.namespace.as_deref())?;
        run_ingest(self, &effective_token, params).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_pack_kg::KgPack;
    use khive_runtime::VerbRegistryBuilder;
    use khive_types::Namespace;
    use std::sync::Arc;

    /// See `fetch::tests::install_web_edge_rules` for why this is needed:
    /// the in-crate test runtime carries no `VerbRegistry`, so the web
    /// pack's own `EDGE_RULES` are never installed on it by default.
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

    // A5: ingest of a served tree on disk under a declared origin mints one
    // entity per file, each with the id its URL would deterministically
    // compute to under that origin (D1) — the row-by-row id-equality
    // assertion the ADR calls for, checked against `identity::document_id`
    // directly rather than against a live HTTP ingest of the same tree
    // (this leg has no test HTTP-serving-a-directory fixture available).
    #[tokio::test]
    async fn a5_disk_ingest_mints_ids_matching_the_declared_origin() {
        let (runtime, token, _dir) = test_runtime().await;
        let pack = WebPack::new(runtime.clone());

        let tree = tempfile::tempdir().expect("tree");
        std::fs::write(
            tree.path().join("index.html"),
            b"<html><body>root</body></html>",
        )
        .unwrap();
        std::fs::create_dir(tree.path().join("sub")).unwrap();
        std::fs::write(
            tree.path().join("sub/page.html"),
            b"<html><body>sub</body></html>",
        )
        .unwrap();

        let reply = pack
            .handle_ingest(
                &token,
                json!({
                    "source": tree.path().to_string_lossy(),
                    "origin": "https://served.example.test",
                }),
            )
            .await
            .expect("disk ingest succeeds");
        let ingested = reply["ingested"].as_array().unwrap();
        assert_eq!(ingested.len(), 2, "one entity per file");

        let origin = Url::parse("https://served.example.test").unwrap();
        let canonical_origin = identity::canonicalize(origin);
        let site = identity::site_id(&canonical_origin);
        assert_eq!(reply["site"], site.to_string());

        let expected_index = identity::document_id(
            site,
            &identity::path_and_query(&identity::canonicalize(
                canonical_origin.join("index.html").unwrap(),
            )),
        );
        let expected_sub = identity::document_id(
            site,
            &identity::path_and_query(&identity::canonicalize(
                canonical_origin.join("sub/page.html").unwrap(),
            )),
        );
        let ids: Vec<String> = ingested
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(ids.contains(&expected_index.to_string()));
        assert!(ids.contains(&expected_sub.to_string()));

        let index_entity = runtime
            .entities(&token)
            .unwrap()
            .get_entity(expected_index)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(index_entity.entity_type.as_deref(), Some("page"));
        let sub_entity = runtime
            .entities(&token)
            .unwrap()
            .get_entity(expected_sub)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sub_entity.entity_type.as_deref(), Some("page"));
    }

    #[tokio::test]
    async fn disk_ingest_without_origin_refuses() {
        let (runtime, token, _dir) = test_runtime().await;
        let pack = WebPack::new(runtime.clone());
        let err = pack
            .handle_ingest(&token, json!({ "source": ["https://a.test/", "https://b.test/"], "origin": "https://x.test" }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("source must be a directory path"),
            "{err}"
        );
    }
}
