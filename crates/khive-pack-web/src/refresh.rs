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
//! Single-hop only: unlike `fetch`, this does not follow redirects. D2 lists
//! `refresh` among the operations that can produce `document supersedes
//! document` on a permanent redirect, which a full implementation would
//! need `fetch.rs`'s redirect loop for; flagged as an open question in
//! LEG_B_REPORT.md rather than duplicated here under this leg's time bound.

use std::time::{Duration, Instant};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{Direction, EdgeRelation};
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;

use crate::egress::{self, Refusal, Resolver, SystemResolver};
use crate::fetch::{resolve_effective_token, run_one_hop, HopOutcome};
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
        if let Some(note) = notes.get_note(hit.node_id).await? {
            if latest
                .as_ref()
                .map(|(t, _)| note.created_at > *t)
                .unwrap_or(true)
            {
                latest = Some((note.created_at, note.id));
            }
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

    egress::check_scheme_and_userinfo(&url)?;
    egress::check_allowlist(url.host_str().unwrap_or_default(), cfg)?;
    let host = url
        .host_str()
        .ok_or_else(|| RuntimeError::InvalidInput("url has no host".to_string()))?
        .to_string();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| RuntimeError::InvalidInput("url has no resolvable port".to_string()))?;
    let addr = egress::resolve_and_pin(resolver, &host).await?;
    let client = egress::pinned_client(
        &host,
        addr,
        port,
        deadline.saturating_duration_since(Instant::now()),
    )?;

    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(etag) = properties.get("etag").and_then(Value::as_str) {
        headers.push(("If-None-Match".to_string(), etag.to_string()));
    }
    if let Some(last_modified) = properties.get("last_modified").and_then(Value::as_str) {
        headers.push(("If-Modified-Since".to_string(), last_modified.to_string()));
    }

    let outcome = run_one_hop(
        &client,
        &url,
        reqwest::Method::GET,
        &headers,
        max_bytes,
        deadline,
    )
    .await?;

    settle_refresh(
        runtime,
        token,
        params.id,
        &url_str,
        &stored_content_ref,
        outcome,
    )
    .await
}

/// Everything after a refresh's single hop has an outcome — egress-free on
/// purpose, so tests can drive it from a locally-dialed `run_one_hop` the
/// same way `fetch::settle` is exercised directly, without going through
/// `resolve_and_pin` (which refuses 127.0.0.1 as loopback regardless of
/// which `Resolver` answers it — the address class is checked on the
/// resolved IP itself, not on resolver trust).
async fn settle_refresh(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    url_str: &str,
    stored_content_ref: &str,
    outcome: HopOutcome,
) -> Result<Value, RuntimeError> {
    let previous_receipt = latest_receipt(runtime, token, id).await?;

    let mut changed = false;
    let mut new_content_ref: Option<String> = None;
    if outcome.status != 304 {
        if let Some((buffer, _truncated)) = &outcome.body {
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
            }
            new_content_ref = Some(content_ref_str);
        }
    }

    let request_record = json!({
        "verb": "web.refresh",
        "url": url_str,
        "status": outcome.status,
        "changed": changed,
        "content_ref": new_content_ref,
    });
    let receipt_id = write_receipt(
        runtime,
        token,
        &format!("web.refresh {url_str}"),
        request_record,
        vec![id],
    )
    .await
    .map_err(|error| {
        RuntimeError::Internal(format!("web.refresh: receipt write failed: {error}"))
    })?;

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
        settle_refresh(runtime, token, id, &url_str, &stored_content_ref, outcome).await
    }

    async fn test_runtime() -> (KhiveRuntime, NamespaceToken, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0)
            .expect("fs blob store");
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
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
}
