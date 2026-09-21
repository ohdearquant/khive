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
//! is no HTTP request — so it calls [`crate::fetch::settle_content`]
//! directly, the same identity-resolve/mint/blob-put/patch sequence
//! `fetch::settle` uses for its terminal hop, so there is one row-minting
//! code path for both a fetched and an ingested row.

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

/// Mint/patch the entity for one disk-tree file through
/// [`crate::fetch::settle_content`] — the same identity/mint/blob-put/patch
/// sequence `fetch::settle` uses for its terminal hop, minus redirect
/// handling (files on disk do not redirect) and with a status/type derived
/// from the file itself rather than an HTTP response.
async fn ingest_disk_file(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    origin: &Url,
    relative_path: &str,
    bytes: Vec<u8>,
) -> Result<Uuid, RuntimeError> {
    let target_url = origin.join(relative_path).map_err(|error| {
        RuntimeError::Internal(format!("bad relative path {relative_path:?}: {error}"))
    })?;
    let content_type = guess_content_type(Path::new(relative_path));

    let settled = crate::fetch::settle_content(
        runtime,
        token,
        &target_url,
        Some(content_type),
        200,
        None,
        None,
        Some((bytes, false)),
    )
    .await?;

    let canonical = identity::canonicalize(target_url);
    let request_record = json!({
        "verb": "web.ingest",
        "mode": "disk",
        "url": canonical.to_string(),
        "bytes": settled.bytes,
    });
    write_receipt(
        runtime,
        token,
        &format!("web.ingest (disk) {canonical}"),
        request_record,
        vec![settled.id],
    )
    .await?;
    Ok(settled.id)
}

/// Refuse visible symlink components before resolving an absolute path.
/// This path-based check still races with replacement before a later open;
/// it is not descriptor-based confinement. Callers retain the checked
/// canonical path instead of opening the original spelling again.
fn canonical_or_refuse(path: &Path, what: &str) -> Result<std::path::PathBuf, RuntimeError> {
    let mut prefix = std::path::PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&prefix).map_err(|error| {
            RuntimeError::from(Refusal::new(
                "ingest_path_unresolvable",
                format!(
                    "web.ingest: cannot inspect {what} {}: {error}",
                    prefix.display()
                ),
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(RuntimeError::from(Refusal::new(
                "ingest_symlink_refused",
                format!(
                    "web.ingest: {what} contains a symbolic link at {}",
                    prefix.display()
                ),
            )));
        }
    }
    std::fs::canonicalize(path).map_err(|error| {
        RuntimeError::from(Refusal::new(
            "ingest_path_unresolvable",
            format!(
                "web.ingest: cannot resolve {what} {}: {error}",
                path.display()
            ),
        ))
    })
}

/// Confines disk ingest to the operator's configured `[web] read_roots`.
/// Refuses when the setting is unset (fail closed) or when the canonicalized
/// source directory falls under none of the canonicalized configured roots.
/// Comparison is by path COMPONENT (`Path::starts_with`), never by string
/// prefix, so `/srv/web` does not contain `/srv/web-evil`.
fn confine_to_read_roots(
    cfg: &khive_runtime::engine_config::WebSectionConfig,
    root_path: &Path,
) -> Result<std::path::PathBuf, RuntimeError> {
    if cfg.read_roots.is_empty() {
        return Err(RuntimeError::from(Refusal::new(
            "ingest_disk_no_read_roots",
            "web.ingest: disk ingest is refused because [web] read_roots is unset; \
             configure at least one root to allow it",
        )));
    }
    let canonical_root = canonical_or_refuse(root_path, "the ingest source")?;
    let contained = cfg.read_roots.iter().any(|configured| {
        canonical_or_refuse(Path::new(configured), "a configured read root")
            .map(|canonical_configured| canonical_root.starts_with(&canonical_configured))
            .unwrap_or(false)
    });
    if !contained {
        return Err(RuntimeError::from(Refusal::new(
            "ingest_source_outside_read_roots",
            format!(
                "web.ingest: {} is outside every configured [web] read_roots entry",
                root_path.display()
            ),
        )));
    }
    Ok(canonical_root)
}

fn walk_files(canonical_root: &Path) -> Result<Vec<std::path::PathBuf>, RuntimeError> {
    let mut out = Vec::new();
    let mut stack = vec![canonical_root.to_path_buf()];
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
            // Check each entry and retain only its canonical path for later
            // traversal/read. This still has a check/open race, documented
            // on canonical_or_refuse; no descriptor guarantee is implied.
            let canonical_entry = canonical_or_refuse(&path, "a path under the ingest source")?;
            if !canonical_entry.starts_with(canonical_root) {
                return Err(RuntimeError::from(Refusal::new(
                    "ingest_symlink_escapes_root",
                    format!(
                        "web.ingest: {} resolves to {}, outside the ingest root — refused",
                        path.display(),
                        canonical_entry.display()
                    ),
                )));
            }
            if canonical_entry.is_dir() {
                stack.push(canonical_entry);
            } else {
                out.push(canonical_entry);
            }
        }
    }
    out.sort();
    Ok(out)
}

async fn ingest_disk(
    pack: &WebPack,
    token: &NamespaceToken,
    cfg: &khive_runtime::engine_config::WebSectionConfig,
    root: &str,
    origin: &str,
    limit: u32,
) -> Result<Value, RuntimeError> {
    let runtime = &pack.runtime;
    let origin_url = Url::parse(origin)
        .map_err(|error| RuntimeError::InvalidInput(format!("invalid origin: {error}")))?;
    let canonical_origin = identity::canonicalize(origin_url.clone());
    let root_path = Path::new(root);
    let canonical_root = confine_to_read_roots(cfg, root_path)?;
    if !canonical_root.is_dir() {
        return Err(RuntimeError::InvalidInput(format!(
            "web.ingest: {root:?} is not a directory"
        )));
    }
    let files = walk_files(&canonical_root)?;
    let site_id = crate::fetch::canonical_site(runtime, token, &canonical_origin).await?;
    let mut minted = Vec::new();
    for path in files.into_iter().take(limit as usize) {
        let relative = path
            .strip_prefix(&canonical_root)
            .map_err(|error| RuntimeError::Internal(error.to_string()))?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let bytes = std::fs::read(&path).map_err(|error| {
            RuntimeError::Internal(format!(
                "web.ingest: cannot read {}: {error}",
                path.display()
            ))
        })?;
        let id = ingest_disk_file(runtime, token, &canonical_origin, &relative, bytes).await?;
        pack.handle_extract(token, json!({ "id": id, "kinds": ["links"] }))
            .await?;
        minted.push(id.to_string());
    }
    Ok(json!({ "mode": "disk", "site": site_id.to_string(), "ingested": minted }))
}

/// One fetch of one address on behalf of the crawl. The reply carries the
/// persisted entity id under `id`, exactly as `web.fetch` reports it.
type FetchReply<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, RuntimeError>> + Send + 'a>>;

/// The production per-address fetch: `web.fetch` with `persist` set, through
/// the same dispatch a caller of the verb would take.
fn fetch_through_web_fetch<'a>(
    pack: &'a WebPack,
    token: &'a NamespaceToken,
) -> impl Fn(String) -> FetchReply<'a> + Sync + 'a {
    move |url: String| -> FetchReply<'a> {
        Box::pin(async move {
            pack.handle_fetch(token, json!({ "url": url, "persist": true }))
                .await
        })
    }
}

async fn ingest_urls(
    pack: &WebPack,
    token: &NamespaceToken,
    urls: Vec<String>,
    depth: u32,
    limit: u32,
) -> Result<Value, RuntimeError> {
    let fetch_one = fetch_through_web_fetch(pack, token);
    crawl(pack, token, urls, depth, limit, &fetch_one).await
}

/// The crawl proper: queue, visited set, per-address fetch, `links`
/// extraction on each persisted row, and the `links_to` walk that feeds the
/// next level. `fetch_one` is the only side of it that reaches the network,
/// which is what lets a test drive the whole crawl against a local listener
/// while production goes through `web.fetch` and its egress guards.
async fn crawl<'a, 'f>(
    pack: &'a WebPack,
    token: &'a NamespaceToken,
    urls: Vec<String>,
    depth: u32,
    limit: u32,
    fetch_one: &'f (dyn Fn(String) -> FetchReply<'a> + Sync + 'f),
) -> Result<Value, RuntimeError>
where
    'a: 'f,
{
    let mut queue: VecDeque<(String, u32)> = urls.into_iter().map(|u| (u, 0)).collect();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ingested = Vec::new();
    let mut refused: Vec<Value> = Vec::new();

    while let Some((url_str, level)) = queue.pop_front() {
        if ingested.len() >= limit as usize {
            break;
        }
        if !visited.insert(url_str.clone()) {
            continue;
        }
        let fetch_reply = fetch_one(url_str.clone()).await;
        let fetch_reply = match fetch_reply {
            Ok(reply) => reply,
            Err(error) => {
                refused.push(json!({ "url": url_str, "error": error.to_string() }));
                continue;
            }
        };
        let Some(id_str) = fetch_reply["id"].as_str() else {
            refused.push(json!({
                "url": url_str,
                "error": "web.fetch reported success with no persisted id",
            }));
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
    Ok(json!({ "mode": "urls", "ingested": ingested, "refused": refused }))
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
        return ingest_disk(pack, token, &pack.runtime.config().web, root, origin, limit).await;
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
    use khive_runtime::engine_config::WebSectionConfig;
    use khive_runtime::{RuntimeConfig, VerbRegistryBuilder};
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
        test_runtime_with_read_roots(Vec::new()).await
    }

    /// Same as `test_runtime`, but with `[web] read_roots` populated —
    /// disk ingest refuses unconditionally against the bare
    /// runtime `test_runtime` builds, so every disk-mode test needs this
    /// form instead.
    async fn test_runtime_with_read_roots(
        read_roots: Vec<String>,
    ) -> (KhiveRuntime, NamespaceToken, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0)
            .expect("fs blob store");
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(dir.path().join("web.db")),
            actor_id: None,
            web: WebSectionConfig {
                read_roots,
                ..Default::default()
            },
            ..RuntimeConfig::no_embeddings()
        })
        .expect("file-backed runtime");
        install_web_edge_rules(&runtime);
        runtime
            .install_blob_store(Arc::new(store))
            .expect("install blob store");
        let token = runtime.authorize(Namespace::local()).expect("authorize");
        (runtime, token, dir)
    }

    // The path policy refuses visible symlink components, including OS aliases
    // such as /var. Supply the real temporary-directory path in the controls.
    fn disk_path(path: &Path) -> String {
        path.canonicalize().unwrap().to_str().unwrap().to_string()
    }

    async fn graph_snapshot(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
    ) -> (Value, Vec<(Uuid, String, Uuid)>) {
        let mut entities = runtime
            .list_entities(token, None, None, 100, 0)
            .await
            .unwrap();
        entities.sort_by_key(|entity| entity.id);
        let ids: std::collections::HashSet<_> = entities.iter().map(|entity| entity.id).collect();
        let mut edges: Vec<_> = runtime
            .list_edges(token, Default::default(), 100, 0)
            .await
            .unwrap()
            .into_iter()
            .filter(|edge| ids.contains(&edge.source_id) && ids.contains(&edge.target_id))
            .map(|edge| (edge.source_id, edge.relation.to_string(), edge.target_id))
            .collect();
        edges.sort();
        let entities: Vec<_> = entities
            .into_iter()
            .map(|entity| {
                let mut properties = entity.properties.unwrap_or(Value::Null);
                if let Some(properties) = properties.as_object_mut() {
                    properties.remove("fetched_at");
                }
                json!({"id": entity.id, "kind": entity.kind, "entity_type": entity.entity_type,
                    "name": entity.name, "properties": properties})
            })
            .collect();
        (json!(entities), edges)
    }

    async fn assert_no_records(runtime: &KhiveRuntime, token: &NamespaceToken) {
        assert!(runtime
            .list_entities(token, None, None, 100, 0)
            .await
            .unwrap()
            .is_empty());
        assert!(runtime
            .list_edges(token, Default::default(), 100, 0)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            runtime
                .notes(token)
                .unwrap()
                .count_notes(token.namespace().as_str(), None)
                .await
                .unwrap(),
            0
        );
    }

    // A5: ingest of a served tree on disk under a declared origin mints one
    // entity per file, each with the id its URL would deterministically
    // compute to under that origin (D1) — the row-by-row id-equality
    // assertion the ADR calls for, checked against `identity::document_id`
    // directly. `a5_literal_http_served_tree_parity...` below is the fuller
    // form: the same tree served over a real HTTP listener, compared
    // against this disk ingest's own output rather than a recomputed id.
    #[tokio::test]
    async fn a5_disk_ingest_mints_ids_matching_the_declared_origin() {
        let tree = tempfile::tempdir().expect("tree");
        let (runtime, token, _dir) =
            test_runtime_with_read_roots(vec![disk_path(tree.path())]).await;
        let pack = WebPack::new(runtime.clone());

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
                    "source": disk_path(tree.path()),
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

    // Serves a small tree over HTTP/1.1 on a loopback port: each request's
    // path selects the body, unknown paths answer 404. Accepts any number of
    // connections, one request each.
    async fn spawn_http_tree_server(files: std::collections::HashMap<String, Vec<u8>>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let files = std::sync::Arc::new(files);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let files = files.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let response = match files.get(&path) {
                        Some(body) => {
                            let mut head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            head.extend_from_slice(body);
                            head
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        port
    }

    // The per-address fetch handed to the crawl in the test above: real bytes
    // off the loopback listener, settled under the declared address through
    // the same `settle_content` path `web.fetch` uses.
    fn served_fetch<'a>(
        runtime: &'a KhiveRuntime,
        token: &'a NamespaceToken,
        port: u16,
    ) -> impl Fn(String) -> super::FetchReply<'a> + Sync + 'a {
        move |url: String| -> super::FetchReply<'a> {
            Box::pin(async move {
                let declared = Url::parse(&url).expect("declared url parses");
                let local_url =
                    Url::parse(&format!("http://127.0.0.1:{port}{}", declared.path())).unwrap();
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .expect("client builds");
                let outcome = crate::fetch::run_one_hop(
                    &client,
                    &local_url,
                    reqwest::Method::GET,
                    &[],
                    10_000,
                    std::time::Instant::now() + std::time::Duration::from_secs(5),
                )
                .await?;
                let content_type = outcome
                    .headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let settled = crate::fetch::settle_content(
                    runtime,
                    token,
                    &declared,
                    content_type.as_deref(),
                    outcome.status,
                    None,
                    None,
                    outcome.body,
                )
                .await?;
                Ok(json!({ "id": settled.id.to_string() }))
            })
        }
    }

    // A5 compares independent databases. Only the HTTP fetch is supplied:
    // it reads real local HTTP bytes, then settles under the declared origin.
    // Both arms use the production links extraction, with receipts excluded
    // from the entity-to-entity graph comparison.
    #[tokio::test]
    async fn a5_literal_http_served_tree_parity_id_and_edge_set_equality_with_disk_ingest() {
        let tree = tempfile::tempdir().expect("tree");
        let (disk_runtime, disk_token, _disk_dir) =
            test_runtime_with_read_roots(vec![disk_path(tree.path())]).await;
        let disk_pack = WebPack::new(disk_runtime.clone());
        let (http_runtime, http_token, _http_dir) = test_runtime().await;
        let http_pack = WebPack::new(http_runtime.clone());

        let index: &[u8] =
            b"<html><a href=\"sub/page.html\">sub</a><a href=\"about.html\">about</a></html>";
        let sub: &[u8] = b"<html><body>sub</body></html>";
        let about: &[u8] = b"<html><body>about</body></html>";
        std::fs::write(tree.path().join("index.html"), index).unwrap();
        std::fs::create_dir(tree.path().join("sub")).unwrap();
        std::fs::write(tree.path().join("sub/page.html"), sub).unwrap();
        std::fs::write(tree.path().join("about.html"), about).unwrap();
        let origin = "https://served.example.test";
        let disk_reply = disk_pack
            .handle_ingest(
                &disk_token,
                json!({"source": disk_path(tree.path()), "origin": origin}),
            )
            .await
            .expect("disk ingest succeeds");
        let disk_graph = graph_snapshot(&disk_runtime, &disk_token).await;
        assert_eq!(
            disk_graph.0.as_array().unwrap().len(),
            4,
            "site and three pages only"
        );
        assert_eq!(disk_graph.1.len(), 5, "three contains edges and two links");
        assert_eq!(
            disk_graph
                .1
                .iter()
                .filter(|(_, relation, _)| relation == "links_to")
                .count(),
            2
        );
        assert!(graph_snapshot(&http_runtime, &http_token)
            .await
            .0
            .as_array()
            .unwrap()
            .is_empty());

        let files = std::collections::HashMap::from([
            ("/index.html".to_string(), index.to_vec()),
            ("/sub/page.html".to_string(), sub.to_vec()),
            ("/about.html".to_string(), about.to_vec()),
        ]);
        let port = spawn_http_tree_server(files).await;
        let fetch_one = served_fetch(&http_runtime, &http_token, port);
        let crawl_reply = super::crawl(
            &http_pack,
            &http_token,
            vec![format!("{origin}/index.html")],
            1,
            10,
            &fetch_one,
        )
        .await
        .expect("crawl succeeds");
        assert_eq!(crawl_reply["refused"], json!([]));
        let sorted_ids = |reply: &Value| {
            let mut ids: Vec<_> = reply["ingested"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| id.as_str().unwrap().to_string())
                .collect();
            ids.sort();
            ids
        };
        assert_eq!(sorted_ids(&disk_reply).len(), 3);
        assert_eq!(sorted_ids(&disk_reply), sorted_ids(&crawl_reply));
        assert_eq!(disk_graph, graph_snapshot(&http_runtime, &http_token).await);
        assert_eq!(
            disk_graph,
            graph_snapshot(&disk_runtime, &disk_token).await,
            "HTTP ingestion cannot fill missing writes in the disk graph"
        );
    }

    #[tokio::test]
    async fn disk_ingest_empty_or_zero_limit_creates_only_the_site() {
        for limit in [None, Some(0)] {
            let tree = tempfile::tempdir().unwrap();
            if limit.is_some() {
                std::fs::write(tree.path().join("index.html"), b"<p>not ingested</p>").unwrap();
            }
            let (runtime, token, _dir) =
                test_runtime_with_read_roots(vec![disk_path(tree.path())]).await;
            let pack = WebPack::new(runtime.clone());
            let reply = pack.handle_ingest(&token, json!({
                "source": disk_path(tree.path()), "origin": "https://empty.example.test", "limit": limit
            })).await.unwrap();
            assert_eq!(reply["mode"], "disk");
            assert_eq!(reply["ingested"], json!([]));
            let (entities, edges) = graph_snapshot(&runtime, &token).await;
            let entities = entities.as_array().unwrap();
            assert_eq!(entities.len(), 1, "no unfetched root-URL resource");
            assert_eq!(entities[0]["id"], reply["site"]);
            assert_eq!(entities[0]["entity_type"], "site");
            assert!(edges.is_empty());
        }
    }

    // disk ingest confinement (`[web] read_roots`): a source directory that
    // IS a configured root, or falls under one, is allowed.
    #[tokio::test]
    async fn read_roots_disk_ingest_inside_a_configured_root_succeeds() {
        let tree = tempfile::tempdir().expect("tree");
        let (runtime, token, _dir) =
            test_runtime_with_read_roots(vec![disk_path(tree.path())]).await;
        let pack = WebPack::new(runtime.clone());
        std::fs::write(tree.path().join("a.html"), b"<html>a</html>").unwrap();

        let reply = pack
            .handle_ingest(
                &token,
                json!({ "source": disk_path(tree.path()), "origin": "https://inside.example.test" }),
            )
            .await
            .expect("a source directory under a configured read_roots entry is allowed");
        assert_eq!(reply["ingested"].as_array().unwrap().len(), 1);
    }

    // Empty `[web] read_roots` fails closed — disk ingest is refused
    // entirely until an operator names at least one root, matching
    // `[exec] read_roots`'s own precedent.
    #[tokio::test]
    async fn read_roots_empty_refuses_disk_ingest_entirely() {
        let tree = tempfile::tempdir().expect("tree");
        std::fs::write(tree.path().join("a.html"), b"<html>a</html>").unwrap();
        let (runtime, token, _dir) = test_runtime().await;
        let pack = WebPack::new(runtime.clone());

        let err = pack
            .handle_ingest(
                &token,
                json!({ "source": disk_path(tree.path()), "origin": "https://x.example.test" }),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("read_roots"),
            "refusal names the setting an operator needs to configure: {err}"
        );
        assert_no_records(&runtime, &token).await;
    }

    // A source directory outside every configured root is refused, even
    // though the directory itself is real and readable — read_roots is an
    // allow-list, not merely a check that the path exists.
    #[tokio::test]
    async fn read_roots_source_outside_every_configured_root_refuses() {
        let allowed_root = tempfile::tempdir().expect("allowed root");
        let other_dir = tempfile::tempdir().expect("a real, but non-configured, directory");
        std::fs::write(other_dir.path().join("a.html"), b"<html>a</html>").unwrap();
        let (runtime, token, _dir) =
            test_runtime_with_read_roots(vec![disk_path(allowed_root.path())]).await;
        let pack = WebPack::new(runtime.clone());

        let err = pack
            .handle_ingest(
                &token,
                json!({ "source": disk_path(other_dir.path()), "origin": "https://x.example.test" }),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("outside every configured"),
            "{err}"
        );
        assert_no_records(&runtime, &token).await;
    }

    // A symlink inside an allowed root pointing outside it is refused at
    // walk time — read_roots confines the whole tree the walk visits, not
    // only the entry point named in `source`.
    #[tokio::test]
    #[cfg(unix)]
    async fn read_roots_symlink_escaping_the_root_refuses() {
        let allowed_root = tempfile::tempdir().expect("allowed root");
        let outside = tempfile::tempdir().expect("outside target");
        std::fs::write(outside.path().join("secret.html"), b"<html>secret</html>").unwrap();
        std::os::unix::fs::symlink(outside.path(), allowed_root.path().join("escape"))
            .expect("symlink");
        let (runtime, token, _dir) =
            test_runtime_with_read_roots(vec![disk_path(allowed_root.path())]).await;
        let pack = WebPack::new(runtime.clone());

        let err = pack
            .handle_ingest(
                &token,
                json!({ "source": disk_path(allowed_root.path()), "origin": "https://x.example.test" }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert_no_records(&runtime, &token).await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn disk_ingest_refuses_visible_symlink_components_even_within_read_roots() {
        for alias_location in ["entry", "source", "ancestor", "configured_root"] {
            let tree = tempfile::tempdir().unwrap();
            let root = std::path::PathBuf::from(disk_path(tree.path()));
            std::fs::create_dir_all(root.join("served/nested")).unwrap();
            std::fs::write(root.join("served/index.html"), b"<p>inside</p>").unwrap();
            let mut configured = root.clone();
            let source = match alias_location {
                "entry" => {
                    std::os::unix::fs::symlink(
                        root.join("served/index.html"),
                        root.join("served/alias.html"),
                    )
                    .unwrap();
                    root.join("served")
                }
                "configured_root" => {
                    std::os::unix::fs::symlink(root.join("served"), root.join("alias")).unwrap();
                    configured = root.join("alias");
                    root.join("served")
                }
                _ => {
                    std::os::unix::fs::symlink(root.join("served"), root.join("alias")).unwrap();
                    if alias_location == "ancestor" {
                        root.join("alias/nested")
                    } else {
                        root.join("alias")
                    }
                }
            };
            let (runtime, token, _dir) =
                test_runtime_with_read_roots(vec![configured.to_str().unwrap().to_string()]).await;
            let pack = WebPack::new(runtime.clone());
            let error = pack
                .handle_ingest(
                    &token,
                    json!({
                        "source": source.to_str().unwrap(), "origin": "https://symlink.example.test"
                    }),
                )
                .await
                .unwrap_err();
            let expected = if alias_location == "configured_root" {
                "outside every configured"
            } else {
                "symbolic link"
            };
            assert!(error.to_string().contains(expected), "{error}");
            assert_no_records(&runtime, &token).await;
        }
    }

    #[tokio::test]
    async fn disk_ingest_refuses_invalid_sources_before_writes() {
        let tree = tempfile::tempdir().unwrap();
        let root = std::path::PathBuf::from(disk_path(tree.path()));
        std::fs::write(root.join("file.html"), b"body").unwrap();
        let (runtime, token, _dir) =
            test_runtime_with_read_roots(vec![root.to_str().unwrap().to_string()]).await;
        let pack = WebPack::new(runtime.clone());
        for source in [root.join("file.html"), root.join("missing")] {
            pack.handle_ingest(
                &token,
                json!({
                    "source": source.to_str().unwrap(), "origin": "https://invalid.example.test"
                }),
            )
            .await
            .expect_err("a disk source must resolve to a directory");
            assert_no_records(&runtime, &token).await;
        }
    }

    #[test]
    fn disk_walk_retains_canonical_checked_paths() {
        let tree = tempfile::tempdir().unwrap();
        let root = std::path::PathBuf::from(disk_path(tree.path()));
        std::fs::create_dir(root.join("unused")).unwrap();
        std::fs::create_dir(root.join("served")).unwrap();
        std::fs::write(root.join("served/page.html"), b"body").unwrap();
        let supplied = root.join("unused/../served");
        let canonical = confine_to_read_roots(
            &WebSectionConfig {
                read_roots: vec![root.to_str().unwrap().to_string()],
                ..Default::default()
            },
            &supplied,
        )
        .unwrap();
        assert_eq!(canonical, root.join("served"));
        assert_eq!(
            walk_files(&canonical).unwrap(),
            vec![root.join("served/page.html")]
        );
    }

    // A URL that `web.fetch` refuses (loopback is a hard refusal
    // regardless of allowlist configuration — see the
    // `a5_literal_http_served_tree_parity...` test above) no longer
    // vanishes from a URL crawl: it is named in the reply's `refused`
    // list instead of leaving an empty result indistinguishable from
    // "nothing to ingest".
    #[tokio::test]
    async fn ingest_urls_surfaces_a_per_url_fetch_refusal_in_the_reply() {
        let (runtime, token, _dir) = test_runtime().await;
        let pack = WebPack::new(runtime.clone());

        let reply = pack
            .handle_ingest(
                &token,
                json!({ "source": "http://127.0.0.1:1/never-reached" }),
            )
            .await
            .expect("a URL crawl itself does not fail even when every URL is refused");

        assert_eq!(reply["mode"], "urls");
        assert_eq!(
            reply["ingested"].as_array().unwrap().len(),
            0,
            "the refused URL mints nothing"
        );
        let refused = reply["refused"].as_array().unwrap();
        assert_eq!(
            refused.len(),
            1,
            "the one refused URL is named, not dropped: {reply}"
        );
        assert_eq!(refused[0]["url"], "http://127.0.0.1:1/never-reached");
        let error = refused[0]["error"].as_str().unwrap();
        assert!(
            error.contains("address_loopback"),
            "refusal names why the URL was skipped: {error}"
        );
    }
}
