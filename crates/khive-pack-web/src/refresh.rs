//! `web.refresh(id)` (ADR-191 D3, D4).
//!
//! A conditional re-fetch of an already-fetched document: `If-None-Match`/
//! `If-Modified-Since` are sent from the entity's own stored `etag`/
//! `last_modified` (both already on `egress::ALLOWED_REQUEST_HEADERS`, so no
//! policy change was needed to send them). A `304` response, or a `200`
//! whose body content-addresses to the SAME `ContentRef` already stored
//! (some origins ignore conditional headers), writes a receipt only — no
//! entity patch, no new blob (the blob store's own `put` is idempotent, so
//! "no blob change" falls out of that rather than needing separate logic).
//! A genuinely changed body puts the new blob and patches the entity in
//! place, same as `fetch`. Every refresh receipt chains to the immediately
//! prior one for the same entity via `note supersedes note` (D4's "receipt
//! chain: the history of one resource's fetches"), whether or not the body
//! changed.
//!
//! Follows redirects the same bounded chain `fetch` does, through the same
//! egress checks on every hop — [`crate::fetch::run_hop_chain`], shared
//! rather than a second redirect loop. Every traversed hop gets the same
//! treatment `fetch::settle` gives it ([`crate::fetch::settle_redirect_hops`]):
//! a placeholder row per hop and, on a permanent redirect (301/308),
//! `document supersedes document` (D2). Identity is by address: on a
//! redirect the terminal address's own row receives the body (minted/patched
//! via `settle_content`, same as `fetch::settle`'s terminal hop); the entity
//! the caller asked to refresh (`id`) keeps its own recorded `url`
//! unchanged. The reply's `final_id` names whichever row actually received
//! the content — `id` itself when there was no redirect, the terminal row
//! otherwise.

use std::time::{Duration, Instant};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{Direction, EdgeRelation};
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;

use crate::egress::{self, Refusal, Resolver, SystemResolver};
use crate::fetch::{resolve_effective_token, HopOutcome};
use crate::receipt::write_receipt;
use crate::WebPack;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RefreshParams {
    id: Uuid,
    #[serde(default)]
    max_bytes: Option<u64>,
    #[serde(default)]
    timeout_s: Option<u64>,
    #[serde(default)]
    namespace: Option<String>,
}

/// A note carries [`crate::receipt::RECEIPT_TAG`] in its
/// `properties["tags"]` array — the only mark distinguishing a receipt from
/// any other `observation` a caller (or a future feature) might annotate the
/// same entity with.
fn is_receipt_note(note: &khive_storage::Note) -> bool {
    note.kind == "observation"
        && note
            .properties
            .as_ref()
            .and_then(|properties| properties.get("tags"))
            .and_then(Value::as_array)
            .is_some_and(|tags| {
                tags.iter()
                    .any(|tag| tag.as_str() == Some(crate::receipt::RECEIPT_TAG))
            })
}

/// The newest `web.receipt`-tagged `observation` annotating `entity_id` —
/// never a decoy `annotates` note a caller wrote by hand, since only the
/// receipt-tag/kind pair identifies a row this function may chain onto or
/// supersede.
async fn latest_receipt(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    entity_id: Uuid,
) -> Result<Option<Uuid>, RuntimeError> {
    let annotators = runtime
        .neighbors(
            token,
            entity_id,
            Direction::In,
            None,
            Some(vec![EdgeRelation::Annotates]),
        )
        .await?;
    if annotators.is_empty() {
        return Ok(None);
    }
    let notes = runtime.notes(token)?;
    let mut latest: Option<(i64, Uuid)> = None;
    for hit in annotators {
        let Some(note) = notes.get_note(hit.node_id).await? else {
            continue;
        };
        if !is_receipt_note(&note) {
            continue;
        }
        if latest
            .as_ref()
            .map(|(t, _)| note.created_at > *t)
            .unwrap_or(true)
        {
            latest = Some((note.created_at, note.id));
        }
    }
    Ok(latest.map(|(_, id)| id))
}

async fn run_refresh(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    resolver: &dyn Resolver,
    cfg: &khive_runtime::engine_config::WebSectionConfig,
    params: RefreshParams,
) -> Result<Value, RuntimeError> {
    let entities = runtime.entities(token)?;
    let entity = entities.get_entity(params.id).await?.ok_or_else(|| {
        RuntimeError::from(Refusal::new(
            "not_found",
            format!("web.refresh: no document at id {}", params.id),
        ))
    })?;
    let properties = entity.properties.clone().unwrap_or(Value::Null);
    let stored_content_ref = properties
        .get("blob_ref")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RuntimeError::from(Refusal::new(
                "not_fetched",
                format!(
                    "web.refresh: {} has no stored body; fetch it first",
                    params.id
                ),
            ))
        })?
        .to_string();
    let url_str = properties
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| RuntimeError::Internal("stored document has no url property".to_string()))?
        .to_string();
    let url = Url::parse(&url_str)
        .map_err(|error| RuntimeError::Internal(format!("stored url is invalid: {error}")))?;

    let ceilings = egress::resolve_ceilings(cfg);
    let max_bytes = egress::check_ceiling(
        params.max_bytes,
        ceilings.max_bytes_default,
        ceilings.max_bytes_max,
        "max_bytes",
    )?;
    let timeout_s = egress::check_ceiling(
        params.timeout_s,
        ceilings.timeout_default_s,
        ceilings.timeout_max_s,
        "timeout_s",
    )?;
    let deadline = Instant::now() + Duration::from_secs(timeout_s);

    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(etag) = properties.get("etag").and_then(Value::as_str) {
        headers.push(("If-None-Match".to_string(), etag.to_string()));
    }
    if let Some(last_modified) = properties.get("last_modified").and_then(Value::as_str) {
        headers.push(("If-Modified-Since".to_string(), last_modified.to_string()));
    }

    // Same bounded chain, same per-hop egress checks as `web.fetch`
    // (`crate::fetch::run_hop_chain`) — a refresh that hits a moved resource
    // follows the redirect rather than conditional-GETing the old address
    // forever. The conditional headers are resent unchanged on every hop,
    // same as `web.fetch` resends its own fixed header set per hop.
    let (outcome, redirect_hops) = crate::fetch::run_hop_chain(
        resolver,
        cfg,
        url,
        reqwest::Method::GET,
        max_bytes,
        deadline,
        |_current_url| Ok(headers.clone()),
    )
    .await?;

    settle_refresh(
        runtime,
        token,
        params.id,
        &url_str,
        &stored_content_ref,
        outcome,
        &redirect_hops,
    )
    .await
}

/// Everything after a refresh's hop chain has an outcome — egress-free on
/// purpose, so tests can drive it from locally-dialed `run_one_hop` calls the
/// same way `fetch::settle` is exercised directly, without going through
/// `resolve_and_pin` (which refuses 127.0.0.1 as loopback regardless of
/// which `Resolver` answers it — the address class is checked on the
/// resolved IP itself, not on resolver trust).
///
/// `redirect_hops` gets the exact same treatment `fetch::settle` gives it
/// (`crate::fetch::settle_redirect_hops`, shared, not duplicated): a
/// placeholder row per traversed hop and `new supersedes old` on 301/308.
/// `id` — the entity the caller asked to refresh — is patched in place only
/// when there was no redirect; on a redirect the terminal address's own row
/// receives the body instead (identity is by address), `id` keeps its own
/// recorded `url`, and the reply's `final_id` names the terminal row.
async fn settle_refresh(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    url_str: &str,
    stored_content_ref: &str,
    outcome: HopOutcome,
    redirect_hops: &[crate::fetch::RedirectHop],
) -> Result<Value, RuntimeError> {
    let previous_receipt = latest_receipt(runtime, token, id).await?;

    let mut entities_touched: Vec<Uuid> = vec![id];
    entities_touched
        .extend(crate::fetch::settle_redirect_hops(runtime, token, redirect_hops).await?);

    let final_url_str = outcome.final_url.to_string();

    let mut changed = false;
    let mut new_content_ref: Option<String> = None;
    let mut final_id: Option<Uuid> = None;
    if outcome.status != 304 {
        if let Some((buffer, truncated)) = &outcome.body {
            let store = crate::blob_store(runtime)?;
            let content_ref = store
                .put(buffer.clone())
                .await
                .map_err(RuntimeError::from)?;
            let content_ref_str = content_ref.to_string();
            if content_ref_str != stored_content_ref {
                changed = true;
                let content_type = outcome
                    .headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                if redirect_hops.is_empty() {
                    let entity_type = match content_type.as_deref() {
                        Some(ct) => {
                            let base = ct
                                .split(';')
                                .next()
                                .unwrap_or_default()
                                .trim()
                                .to_ascii_lowercase();
                            if base == "text/html" || base == "application/xhtml+xml" {
                                "page"
                            } else {
                                "resource"
                            }
                        }
                        None => "resource",
                    };
                    crate::entities::patch(
                        runtime,
                        token,
                        id,
                        Some(entity_type),
                        json!({
                            "url": url_str,
                            "content_type": content_type,
                            "blob_ref": content_ref_str,
                            "content_digest": content_ref_str,
                            "size": buffer.len() as u64,
                            "status": outcome.status,
                            "fetched_at": chrono::Utc::now().to_rfc3339(),
                            "etag": outcome.headers.get("etag").and_then(|v| v.to_str().ok()),
                            "last_modified": outcome.headers.get("last-modified").and_then(|v| v.to_str().ok()),
                        }),
                    )
                    .await?;
                    crate::fetch::root_body(
                        runtime,
                        id,
                        khive_storage::AttachmentSubstrate::Entity,
                        &content_ref,
                        content_type.as_deref(),
                        buffer.len() as u64,
                    )
                    .await?;
                } else {
                    // Identity is by address: the body served at the terminal
                    // hop belongs to that address's own row, which
                    // `settle_redirect_hops` has already minted (and, on
                    // 301/308, linked `supersedes` over the caller's row).
                    // The caller-named row keeps its own url.
                    let settled = crate::fetch::settle_content(
                        runtime,
                        token,
                        &outcome.final_url,
                        content_type.as_deref(),
                        outcome.status,
                        outcome.headers.get("etag").and_then(|v| v.to_str().ok()),
                        outcome
                            .headers
                            .get("last-modified")
                            .and_then(|v| v.to_str().ok()),
                        Some((buffer.clone(), *truncated)),
                    )
                    .await?;
                    if !entities_touched.contains(&settled.id) {
                        entities_touched.push(settled.id);
                    }
                    final_id = Some(settled.id);
                }
            }
            new_content_ref = Some(content_ref_str);
        }
    }

    let redirect_chain: Vec<Value> = redirect_hops
        .iter()
        .map(|hop| {
            json!({ "from": hop.from.to_string(), "to": hop.to.to_string(), "status": hop.status })
        })
        .collect();

    let request_record = json!({
        "verb": "web.refresh",
        "url": url_str,
        "final_url": final_url_str,
        "status": outcome.status,
        "changed": changed,
        "content_ref": new_content_ref,
        "redirects": redirect_hops.len() as u32,
        "redirect_chain": redirect_chain,
    });
    let receipt_id = write_receipt(
        runtime,
        token,
        &format!("web.refresh {url_str}"),
        request_record,
        entities_touched,
    )
    .await
    .map_err(|error| {
        RuntimeError::Internal(format!("web.refresh: receipt write failed: {error}"))
    })?;
    if let Some(content_ref) = &new_content_ref {
        crate::fetch::root_body(
            runtime,
            receipt_id,
            khive_storage::AttachmentSubstrate::Note,
            &khive_storage::ContentRef::from_hex(content_ref.clone()).map_err(|error| {
                RuntimeError::Internal(format!("content_ref {content_ref:?} unparseable: {error}"))
            })?,
            outcome
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            outcome
                .body
                .as_ref()
                .map(|(b, _)| b.len() as u64)
                .unwrap_or(0),
        )
        .await?;
    }

    if let Some(previous) = previous_receipt {
        runtime
            .link(
                token,
                receipt_id,
                previous,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
            .await?;
    }

    Ok(json!({
        "id": id.to_string(),
        "status": outcome.status,
        "changed": changed,
        "receipt_id": receipt_id.to_string(),
        "redirects": redirect_hops.len() as u32,
        "final_id": final_id.map(|value| value.to_string()),
    }))
}

impl WebPack {
    pub(crate) async fn handle_refresh(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let params: RefreshParams = serde_json::from_value(params).map_err(|error| {
            RuntimeError::InvalidInput(format!("invalid web.refresh arguments: {error}"))
        })?;
        let effective_token = resolve_effective_token(token, params.namespace.as_deref())?;
        run_refresh(
            &self.runtime,
            &effective_token,
            &SystemResolver,
            &self.runtime.config().web,
            params,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::run_one_hop;
    use crate::identity;
    use khive_types::Namespace;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn plain_client(timeout: Duration) -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .expect("plain client builds")
    }

    /// Drives the same post-egress path `run_refresh` uses (`run_one_hop`
    /// then `settle_refresh`) directly against a local listener, bypassing
    /// `resolve_and_pin` — see `settle_refresh`'s doc comment for why no
    /// `Resolver` override can reach a loopback test server instead.
    async fn run_refresh_local(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        id: Uuid,
        headers: &[(String, String)],
    ) -> Result<Value, RuntimeError> {
        let entities = runtime.entities(token).unwrap();
        let entity = entities.get_entity(id).await.unwrap().unwrap();
        let properties = entity.properties.clone().unwrap_or(Value::Null);
        let stored_content_ref = properties["blob_ref"]
            .as_str()
            .expect("seeded entity carries blob_ref")
            .to_string();
        let url_str = properties["url"]
            .as_str()
            .expect("seeded entity carries url")
            .to_string();
        let url = Url::parse(&url_str).expect("valid stored url");
        let client = plain_client(Duration::from_secs(5));
        let outcome = run_one_hop(
            &client,
            &url,
            reqwest::Method::GET,
            headers,
            10_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await?;
        settle_refresh(
            runtime,
            token,
            id,
            &url_str,
            &stored_content_ref,
            outcome,
            &[],
        )
        .await
    }

    /// The in-crate test runtime carries no `VerbRegistry`, so the web
    /// pack's own `EDGE_RULES` (`site contains page|resource`) are never
    /// installed on it by default — a redirect-hop `link(Contains, ...)` (via
    /// `mint_bare`) then refuses against the base allowlist alone. Register
    /// kg+web through a throwaway registry purely to read back their
    /// combined `all_edge_rules()`, matching `fetch.rs`'s own test helper of
    /// the same name.
    fn install_web_edge_rules(runtime: &KhiveRuntime) {
        let mut builder = khive_runtime::VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register(WebPack::new(runtime.clone()));
        let registry = builder.build().expect("registry builds");
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

    fn http_response(
        status: u16,
        reason: &str,
        extra_headers: &[(&str, String)],
        body: &[u8],
    ) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
        for (name, value) in extra_headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
        out.extend_from_slice(b"Connection: close\r\n\r\n");
        out.extend_from_slice(body);
        out
    }

    async fn spawn_once(response: Vec<u8>) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = hits.clone();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                hits_task.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(&response).await;
                let _ = stream.shutdown().await;
            }
        });
        (port, hits)
    }

    async fn seed(runtime: &KhiveRuntime, token: &NamespaceToken, port: u16, body: &[u8]) -> Uuid {
        let url = Url::parse(&format!("http://127.0.0.1:{port}/r")).unwrap();
        let canonical = identity::canonicalize(url);
        let site = identity::site_id(&canonical);
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
        let store = crate::blob_store(runtime).unwrap();
        let content_ref = store.put(body.to_vec()).await.unwrap();
        crate::entities::get_or_create(
            runtime,
            token,
            id,
            "document",
            "resource",
            canonical.as_ref(),
            json!({ "url": canonical.to_string() }),
        )
        .await
        .unwrap();
        crate::entities::patch(
            runtime,
            token,
            id,
            Some("resource"),
            json!({ "url": canonical.to_string(), "blob_ref": content_ref.to_string() }),
        )
        .await
        .unwrap();
        id
    }

    // A4: refresh whose response content-addresses to the SAME bytes
    // already stored writes no entity or blob change, only a receipt (and
    // chains to the previous receipt); a changed body is the control that
    // updates the blob ref and entity properties, and its receipt chains
    // too. Two independent entities/listeners — a one-shot listener cannot
    // serve both scenarios.
    #[tokio::test]
    async fn a4_refresh_unchanged_writes_receipt_only_changed_body_updates_control() {
        let (runtime, token, _dir) = test_runtime().await;

        // Unchanged: the server returns the identical body.
        let body = b"stable body".to_vec();
        let (port, hits) = spawn_once(http_response(200, "OK", &[], &body)).await;
        let id = seed(&runtime, &token, port, &body).await;
        let before = runtime
            .entities(&token)
            .unwrap()
            .get_entity(id)
            .await
            .unwrap()
            .unwrap();
        let before_ref = before.properties.clone().unwrap()["blob_ref"]
            .as_str()
            .unwrap()
            .to_string();
        let before_updated_at = before.updated_at;

        let reply = run_refresh_local(&runtime, &token, id, &[])
            .await
            .expect("refresh dispatches");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "exactly one conditional GET was made"
        );
        assert_eq!(reply["changed"], false);
        assert!(reply["receipt_id"].is_string());

        let after = runtime
            .entities(&token)
            .unwrap()
            .get_entity(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.properties.unwrap()["blob_ref"].as_str().unwrap(),
            before_ref,
            "unchanged digest leaves blob_ref untouched"
        );
        assert_eq!(
            after.updated_at, before_updated_at,
            "no entity write on an unchanged refresh"
        );

        // Changed: a different body updates the blob ref and content_type.
        let new_body = b"a genuinely different body".to_vec();
        let (port2, hits2) = spawn_once(http_response(
            200,
            "OK",
            &[("Content-Type", "text/plain".to_string())],
            &new_body,
        ))
        .await;
        let id2 = seed(&runtime, &token, port2, &body).await;
        let reply2 = run_refresh_local(&runtime, &token, id2, &[])
            .await
            .expect("refresh dispatches");
        assert_eq!(hits2.load(Ordering::SeqCst), 1);
        assert_eq!(reply2["changed"], true);
        let after2 = runtime
            .entities(&token)
            .unwrap()
            .get_entity(id2)
            .await
            .unwrap()
            .unwrap();
        let after2_props = after2.properties.unwrap();
        assert_eq!(after2_props["content_type"], "text/plain");
        let new_ref = after2_props["blob_ref"].as_str().unwrap();
        let store = crate::blob_store(&runtime).unwrap();
        let stored = store
            .get_bounded_verified(
                &khive_storage::ContentRef::from_hex(new_ref).unwrap(),
                khive_storage::MAX_BLOB_WHOLE_BYTES,
            )
            .await
            .unwrap();
        assert_eq!(stored, new_body);
    }

    // D4: a second refresh's receipt supersedes the first, chaining the
    // history of this resource's refreshes.
    #[tokio::test]
    async fn refresh_receipt_chains_to_previous_via_supersedes() {
        let (runtime, token, _dir) = test_runtime().await;
        let body = b"same every time".to_vec();
        let (port, _hits) = spawn_once(http_response(200, "OK", &[], &body)).await;
        let id = seed(&runtime, &token, port, &body).await;

        let first = run_refresh_local(&runtime, &token, id, &[]).await.unwrap();
        let first_receipt = uuid::Uuid::parse_str(first["receipt_id"].as_str().unwrap()).unwrap();
        let first_neighbors = runtime
            .neighbors(
                &token,
                first_receipt,
                khive_storage::Direction::Out,
                None,
                Some(vec![EdgeRelation::Supersedes]),
            )
            .await
            .unwrap();
        assert_eq!(
            first_neighbors.len(),
            0,
            "nothing precedes the first refresh"
        );

        // A one-shot listener cannot serve a second request, so the second
        // refresh's fixture binds a fresh port and the stored `url` is
        // repointed at it — this is still "refresh the same entity id
        // again", which is what the chaining behavior is about.
        let (port2, _hits2) = spawn_once(http_response(200, "OK", &[], &body)).await;
        let repointed_url = format!("http://127.0.0.1:{port2}/r");
        crate::entities::patch(&runtime, &token, id, None, json!({ "url": repointed_url }))
            .await
            .unwrap();

        let second = run_refresh_local(&runtime, &token, id, &[]).await.unwrap();
        let second_receipt = uuid::Uuid::parse_str(second["receipt_id"].as_str().unwrap()).unwrap();
        assert_ne!(second_receipt, first_receipt);

        let second_neighbors = runtime
            .neighbors(
                &token,
                second_receipt,
                khive_storage::Direction::Out,
                None,
                Some(vec![EdgeRelation::Supersedes]),
            )
            .await
            .unwrap();
        assert_eq!(
            second_neighbors.len(),
            1,
            "the second refresh's receipt supersedes the first"
        );
        assert_eq!(second_neighbors[0].node_id, first_receipt);
    }

    // `latest_receipt` filters on kind+tag, not just "newest annotates
    // neighbour": a decoy `observation` note annotating the same entity
    // AFTER the real receipt, but carrying no `RECEIPT_TAG`, must never be
    // mistaken for the previous receipt. The next refresh's receipt
    // supersedes the real receipt, and the decoy gets no incoming
    // supersedes edge at all.
    #[tokio::test]
    async fn latest_receipt_ignores_a_decoy_annotating_note_without_the_receipt_tag() {
        let (runtime, token, _dir) = test_runtime().await;
        let body = b"same every time".to_vec();
        let (port, _hits) = spawn_once(http_response(200, "OK", &[], &body)).await;
        let id = seed(&runtime, &token, port, &body).await;

        let first = run_refresh_local(&runtime, &token, id, &[]).await.unwrap();
        let first_receipt = uuid::Uuid::parse_str(first["receipt_id"].as_str().unwrap()).unwrap();

        // Written strictly after the real receipt, so a created-at-only sort
        // would pick it over the real receipt — but it carries no
        // `RECEIPT_TAG`.
        let decoy = runtime
            .create_note(
                &token,
                "observation",
                None,
                "a caller's own note about this page",
                None,
                None,
                vec![id],
            )
            .await
            .unwrap()
            .id;

        let (port2, _hits2) = spawn_once(http_response(200, "OK", &[], &body)).await;
        let repointed_url = format!("http://127.0.0.1:{port2}/r");
        crate::entities::patch(&runtime, &token, id, None, json!({ "url": repointed_url }))
            .await
            .unwrap();

        let second = run_refresh_local(&runtime, &token, id, &[]).await.unwrap();
        let second_receipt = uuid::Uuid::parse_str(second["receipt_id"].as_str().unwrap()).unwrap();

        let second_neighbors = runtime
            .neighbors(
                &token,
                second_receipt,
                khive_storage::Direction::Out,
                None,
                Some(vec![EdgeRelation::Supersedes]),
            )
            .await
            .unwrap();
        assert_eq!(
            second_neighbors.len(),
            1,
            "the second refresh's receipt supersedes exactly one prior note"
        );
        assert_eq!(
            second_neighbors[0].node_id, first_receipt,
            "it supersedes the real receipt, not the decoy"
        );

        let decoy_neighbors = runtime
            .neighbors(
                &token,
                decoy,
                khive_storage::Direction::In,
                None,
                Some(vec![EdgeRelation::Supersedes]),
            )
            .await
            .unwrap();
        assert_eq!(
            decoy_neighbors.len(),
            0,
            "the decoy is never superseded — it was never treated as a receipt"
        );
    }

    // ADR-191 D2/D6: refresh follows redirects the same way fetch does and
    // emits `document supersedes document` on 301/308 — through the shared
    // `crate::fetch::settle_redirect_hops`, exercised
    // here exactly the way `fetch.rs`'s own
    // `a3_permanent_redirect_supersedes_temporary_redirect_no_edge` exercises
    // `fetch::settle`: hand-built `RedirectHop`/`HopOutcome` values, no
    // network. Control: a 302 hop yields no supersedes edge, just a receipt
    // naming the hop.
    #[tokio::test]
    async fn refresh_301_yields_supersedes_edge_302_control_receipt_names_the_hop() {
        let (runtime, token, _dir) = test_runtime().await;

        // 301: settle_refresh must mint the new address and link
        // new supersedes old, exactly like fetch::settle does for the same
        // redirect status.
        let old_url = Url::parse("http://127.0.0.1:40101/r").unwrap();
        let new_url = Url::parse("http://127.0.0.1:40101/moved").unwrap();
        let old_id = seed(&runtime, &token, 40101, b"stale body").await;
        let old_entity = runtime
            .entities(&token)
            .unwrap()
            .get_entity(old_id)
            .await
            .unwrap()
            .unwrap();
        let stored_ref = old_entity.properties.clone().unwrap()["blob_ref"]
            .as_str()
            .unwrap()
            .to_string();
        let hop = crate::fetch::RedirectHop {
            from: old_url.clone(),
            to: new_url.clone(),
            status: 301,
        };
        let outcome = HopOutcome {
            status: 200,
            final_url: new_url.clone(),
            headers: reqwest::header::HeaderMap::new(),
            redirect_to: None,
            body: Some((b"content at the new address".to_vec(), false)),
        };
        let reply = settle_refresh(
            &runtime,
            &token,
            old_id,
            old_url.as_ref(),
            &stored_ref,
            outcome,
            &[hop],
        )
        .await
        .expect("settle_refresh dispatches");
        assert_eq!(reply["redirects"], 1);

        let new_id = identity::document_id(
            identity::site_id(&identity::canonicalize(new_url.clone())),
            &identity::path_and_query(&identity::canonicalize(new_url.clone())),
        );
        let neighbors = runtime
            .neighbors(&token, new_id, khive_storage::Direction::Out, None, None)
            .await
            .unwrap();
        assert!(
            neighbors
                .iter()
                .any(|n| n.node_id == old_id && n.relation == EdgeRelation::Supersedes),
            "301 during refresh must yield new supersedes old, exactly like fetch"
        );

        let receipt_id = uuid::Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
        let receipt = runtime
            .notes(&token)
            .unwrap()
            .get_note(receipt_id)
            .await
            .unwrap()
            .unwrap();
        let request = receipt.properties.unwrap()["request"].clone();
        assert_eq!(request["redirects"], 1);
        assert_eq!(request["redirect_chain"][0]["status"], 301);
        assert_eq!(request["redirect_chain"][0]["to"], new_url.to_string());

        // Control: a 302 hop yields no supersedes edge, just a receipt
        // naming the hop.
        let old_url2 = Url::parse("http://127.0.0.1:40102/r").unwrap();
        let new_url2 = Url::parse("http://127.0.0.1:40102/temp").unwrap();
        let old_id2 = seed(&runtime, &token, 40102, b"stale body two").await;
        let old_entity2 = runtime
            .entities(&token)
            .unwrap()
            .get_entity(old_id2)
            .await
            .unwrap()
            .unwrap();
        let stored_ref2 = old_entity2.properties.clone().unwrap()["blob_ref"]
            .as_str()
            .unwrap()
            .to_string();
        let hop2 = crate::fetch::RedirectHop {
            from: old_url2.clone(),
            to: new_url2.clone(),
            status: 302,
        };
        let outcome2 = HopOutcome {
            status: 200,
            final_url: new_url2.clone(),
            headers: reqwest::header::HeaderMap::new(),
            redirect_to: None,
            body: Some((b"content at the temp address".to_vec(), false)),
        };
        let reply2 = settle_refresh(
            &runtime,
            &token,
            old_id2,
            old_url2.as_ref(),
            &stored_ref2,
            outcome2,
            &[hop2],
        )
        .await
        .expect("settle_refresh dispatches");
        assert_eq!(reply2["redirects"], 1);

        let new_id2 = identity::document_id(
            identity::site_id(&identity::canonicalize(new_url2.clone())),
            &identity::path_and_query(&identity::canonicalize(new_url2.clone())),
        );
        let neighbors2 = runtime
            .neighbors(&token, new_id2, khive_storage::Direction::Out, None, None)
            .await
            .unwrap();
        assert!(
            !neighbors2
                .iter()
                .any(|n| n.relation == EdgeRelation::Supersedes),
            "a 302 during refresh yields no supersedes edge"
        );

        let receipt_id2 = uuid::Uuid::parse_str(reply2["receipt_id"].as_str().unwrap()).unwrap();
        let receipt2 = runtime
            .notes(&token)
            .unwrap()
            .get_note(receipt_id2)
            .await
            .unwrap()
            .unwrap();
        let request2 = receipt2.properties.unwrap()["request"].clone();
        assert_eq!(request2["redirects"], 1);
        assert_eq!(request2["redirect_chain"][0]["status"], 302);
    }
}
