//! `web.fetch` (ADR-191 D1-D4).
//!
//! Split in two layers, deliberately: [`egress`](crate::egress) decides
//! whether a hop is allowed to happen at all (address class, allowlist,
//! credential scope, headers, ceilings) with zero networking; this module's
//! [`run_one_hop`] is the mechanical HTTP execution against an
//! already-decided target, with zero policy judgment. [`run_hop_chain`] wires
//! the two together for fetch, refresh, and HTTP search provider requests.
//!
//! Entity minting (D1/D3) lives in [`settle_with_request_headers`], run once the redirect loop
//! reaches its terminal hop: every hop in the chain — including redirect
//! hops that never carry a body — becomes a `site`-scoped `page`/`resource`
//! row (unfetched placeholders for anything but the terminal hop), a
//! permanent redirect (301/308) becomes `new supersedes old` (D2), and the
//! terminal hop gets the blob, the full property set, and the receipt. The
//! terminal hop's own mint/blob/patch sequence is [`settle_content`], shared
//! with `web.ingest`'s disk-tree path (`crate::ingest::ingest_disk_file`) so
//! there is one row-minting code path for both a fetched and an ingested
//! `page`/`resource` row.

use std::collections::BTreeMap;
use std::time::Instant;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use khive_runtime::engine_config::WebSectionConfig;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::EdgeRelation;
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;

use crate::egress::{self, Refusal, Resolver, SystemResolver};
use crate::identity;
use crate::namespace::resolve_effective_token;
use crate::receipt::write_receipt;
use crate::vocab::FetchParams;
use crate::WebPack;
use khive_storage::{Attachment, AttachmentSubstrate, ContentRef, NewAttachment};

/// Bounded, default five (D3 carries ADR-175 A1.2.4 over unchanged). Not
/// operator-configurable — a fixed default with no override, unlike the
/// byte/time ceilings.
pub const MAX_REDIRECTS: u32 = 5;

const INLINE_BODY_BUDGET: usize = khive_runtime::daemon::MAX_FRAME_BYTES - 4096;
const INLINE_RESULT_BUDGET: usize = khive_runtime::daemon::MAX_FRAME_BYTES - 1024;
const INLINE_RAW_BODY_LIMIT: u64 = (INLINE_BODY_BUDGET / 4 * 3) as u64;

/// Response headers echoed to the caller and recorded in the receipt: a
/// response header set is attacker-controlled, so only this allow-listed
/// subset is ever surfaced.
const ALLOWED_RESPONSE_HEADERS: &[&str] =
    &["content-type", "content-length", "last-modified", "etag"];

pub(crate) fn extract_allowed_headers(headers: &reqwest::header::HeaderMap) -> Value {
    let mut out = serde_json::Map::new();
    for name in ALLOWED_RESPONSE_HEADERS {
        if let Some(value) = headers.get(*name) {
            if let Ok(text) = value.to_str() {
                out.insert((*name).to_string(), Value::String(text.to_string()));
            }
        }
    }
    Value::Object(out)
}

const NEGOTIATION_HEADERS: &[&str] = &["accept", "accept-language"];

/// Keep only representation negotiation, never credentials or conditional
/// validators. Lists retain repeated header values in their sent order.
pub(crate) fn negotiation_headers(headers: &[(String, String)]) -> BTreeMap<String, Vec<String>> {
    let mut selected: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        if NEGOTIATION_HEADERS.contains(&name.as_str()) {
            selected.entry(name).or_default().push(value.clone());
        }
    }
    selected
}

pub(crate) fn stored_negotiation_headers(
    properties: &Value,
) -> Result<Vec<(String, String)>, RuntimeError> {
    let mut headers = Vec::new();
    for name in NEGOTIATION_HEADERS {
        if let Some(value) = properties
            .get("request_headers")
            .and_then(|headers| headers.get(*name))
        {
            let values: Vec<String> = serde_json::from_value(value.clone()).map_err(|error| {
                RuntimeError::InvalidInput(format!("stored {name} negotiation is invalid: {error}"))
            })?;
            headers.extend(values.into_iter().map(|value| ((*name).to_string(), value)));
        }
    }
    Ok(headers)
}

pub(crate) async fn persist_negotiation_headers(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    headers: &[(String, String)],
) -> Result<(), RuntimeError> {
    let entity = runtime
        .entities(token)?
        .get_entity(id)
        .await?
        .ok_or_else(|| RuntimeError::NotFound(id.to_string()))?;
    let properties = entity.properties.unwrap_or(Value::Null);
    let selected = negotiation_headers(headers);
    if negotiation_headers(&stored_negotiation_headers(&properties)?) != selected {
        crate::entities::patch(
            runtime,
            token,
            id,
            None,
            json!({"request_headers": selected}),
        )
        .await?;
    }
    Ok(())
}

fn header_str<'a>(headers: &'a reqwest::header::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// D1: a body is a `page` when its (allow-listed) response `content-type`
/// starts with `text/html` or `application/xhtml+xml`, ignoring any `;
/// charset=...` parameter and case. Everything else — including no header at
/// all — is a `resource`.
pub(crate) fn classify_entity_type(content_type: Option<&str>) -> &'static str {
    let base = content_type
        .and_then(|v| v.split(';').next())
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if base == "text/html" || base == "application/xhtml+xml" {
        "page"
    } else {
        "resource"
    }
}

/// Outcome of one mechanical HTTP hop — no policy content at all.
#[derive(Debug)]
pub(crate) struct HopOutcome {
    pub status: u16,
    pub final_url: Url,
    pub headers: reqwest::header::HeaderMap,
    pub redirect_to: Option<Url>,
    /// `Some` for GET (possibly empty), `None` for HEAD.
    pub body: Option<(Vec<u8>, bool)>,
}

/// Execute exactly one HTTP hop against `client` (already pointed, pinned or
/// not, at wherever it should connect) — no DNS, no address classification,
/// no allowlist, no redirect following. `deadline` bounds this hop (and, by
/// construction of the caller's loop, the whole multi-hop read).
///
/// Redirects are never auto-followed by `client` (callers build it with
/// `redirect::Policy::none()`); a 3xx response with a `Location` header is
/// returned as `redirect_to` for the caller to re-check and re-dial.
pub(crate) async fn run_one_hop(
    client: &reqwest::Client,
    url: &Url,
    method: reqwest::Method,
    headers: &[(String, String)],
    max_bytes: u64,
    deadline: Instant,
) -> Result<HopOutcome, RuntimeError> {
    let deadline = tokio::time::Instant::from_std(deadline);
    if deadline <= tokio::time::Instant::now() {
        return Err(Refusal::new(
            "response_too_slow",
            "time budget exhausted before the request",
        )
        .into());
    }
    let want_body = method == reqwest::Method::GET;
    let mut request = client.request(method, url.clone());
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let hop = async {
        let response = request
            .send()
            .await
            .map_err(|error| RuntimeError::InvalidInput(format!("transport_error: {error}")))?;
        let status = response.status().as_u16();
        let final_url = response.url().clone();
        let response_headers = response.headers().clone();
        let redirect_to = if response.status().is_redirection() {
            response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|location| final_url.join(location).ok())
        } else {
            None
        };
        let body =
            if want_body && redirect_to.is_none() {
                let mut response = response;
                let mut buffer: Vec<u8> = Vec::new();
                let mut truncated = false;
                while let Some(chunk) = response.chunk().await.map_err(|error| {
                    RuntimeError::InvalidInput(format!("transport_error: {error}"))
                })? {
                    if buffer.len() as u64 >= max_bytes {
                        truncated = true;
                        break;
                    }
                    let room = (max_bytes - buffer.len() as u64) as usize;
                    if chunk.len() > room {
                        buffer.extend_from_slice(&chunk[..room]);
                        truncated = true;
                        break;
                    }
                    buffer.extend_from_slice(&chunk);
                }
                Some((buffer, truncated))
            } else {
                None
            };
        Ok::<_, RuntimeError>(HopOutcome {
            status,
            final_url,
            headers: response_headers,
            redirect_to,
            body,
        })
    };
    match tokio::time::timeout_at(deadline, hop).await {
        Ok(result) => result,
        Err(_) => Err(Refusal::new("response_too_slow", "response exceeded the time bound").into()),
    }
}

struct CredentialAttachment {
    header: (String, String),
}

/// One traversed redirect: `from` responded `status` naming `to` as its
/// `Location`. Never the terminal hop — that one is handled by [`settle_with_request_headers`]
/// (or, for `web.refresh`, [`settle_redirect_hops`] directly) from the
/// loop's final outcome. `pub(crate)` so [`crate::refresh`] shares this type
/// rather than declaring an equivalent one of its own.
pub(crate) struct RedirectHop {
    pub(crate) from: Url,
    pub(crate) to: Url,
    pub(crate) status: u16,
}

/// Merge `accept` into `headers` as the Accept header, then validate the
/// combined set through the exact same allow-list [`egress::check_headers`]
/// applies to `headers` alone — `accept` is a convenience top-level param,
/// never a second, unvalidated path to set a request header. Refuses if
/// `accept` and an explicit `headers["Accept"]` (case-insensitive) both try
/// to set it, rather than silently picking one.
pub(crate) fn effective_request_headers(
    headers: &BTreeMap<String, String>,
    accept: Option<&str>,
) -> Result<Vec<(String, String)>, Refusal> {
    let mut combined = headers.clone();
    if let Some(accept) = accept {
        if combined
            .keys()
            .any(|name| name.eq_ignore_ascii_case("accept"))
        {
            return Err(Refusal::new(
                "header_conflict",
                "accept and headers[\"Accept\"] both set the Accept header; set it in one place",
            ));
        }
        combined.insert("Accept".to_string(), accept.to_string());
    }
    egress::check_headers(&combined)
}

async fn run_fetch(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    resolver: &dyn Resolver,
    cfg: &WebSectionConfig,
    params: FetchParams,
    clients: &egress::PinnedClients,
) -> Result<Value, RuntimeError> {
    let method_name = params
        .method
        .as_deref()
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let method = match method_name.as_str() {
        "GET" => reqwest::Method::GET,
        "HEAD" => reqwest::Method::HEAD,
        other => {
            return Err(Refusal::new(
                "method_not_allowed",
                format!("method {other:?} is refused; only GET and HEAD are permitted"),
            )
            .into())
        }
    };
    let url = Url::parse(&params.url)
        .map_err(|error| RuntimeError::InvalidInput(format!("invalid url: {error}")))?;
    let persist = params.persist.unwrap_or(true);

    let ceilings = egress::resolve_ceilings(cfg)?;
    let max_bytes = egress::check_ceiling(
        params.max_bytes,
        ceilings.max_bytes_default,
        ceilings.max_bytes_max,
        "max_bytes",
    )?;
    if !persist && method == reqwest::Method::GET && max_bytes > INLINE_RAW_BODY_LIMIT {
        return Err(RuntimeError::InvalidInput(format!(
            "web.fetch: max_bytes={max_bytes} exceeds the transient inline response budget of {INLINE_RAW_BODY_LIMIT} raw bytes; lower max_bytes or use persist=true"
        )));
    }
    let timeout_s = egress::check_ceiling(
        params.timeout_s,
        ceilings.timeout_default_s,
        ceilings.timeout_max_s,
        "timeout_s",
    )?;
    let allowed_headers = effective_request_headers(&params.headers, params.accept.as_deref())?;

    let credential = match &params.credential {
        None => None,
        Some(name) => {
            let host = url
                .host_str()
                .ok_or_else(|| RuntimeError::InvalidInput("url has no host".to_string()))?;
            let entry = egress::check_credential(cfg, name, host)?;
            let value = std::env::var(&entry.env_var).map_err(|_| {
                RuntimeError::InvalidInput(format!(
                    "credential {name:?}: environment variable {:?} is not set",
                    entry.env_var
                ))
            })?;
            Some(CredentialAttachment {
                header: ("Authorization".to_string(), format!("Bearer {value}")),
            })
        }
    };

    let deadline = egress::request_deadline(timeout_s)?;

    let (outcome, redirect_hops) = run_hop_chain_with_clients(
        clients,
        resolver,
        cfg,
        url.clone(),
        method.clone(),
        max_bytes,
        deadline,
        |current_url| {
            if credential.is_some() {
                egress::check_credential_scheme(current_url)?;
                let host = current_url.host_str().unwrap_or_default();
                egress::check_credential(
                    cfg,
                    params.credential.as_deref().unwrap_or_default(),
                    host,
                )?;
            }
            let mut hop_headers_out: Vec<(String, String)> = allowed_headers.clone();
            if let Some(credential) = &credential {
                hop_headers_out.push(credential.header.clone());
            }
            Ok(hop_headers_out)
        },
    )
    .await?;

    settle_with_request_headers(
        runtime,
        token,
        &method_name,
        &outcome.final_url,
        outcome.status,
        &outcome.headers,
        outcome.body,
        &redirect_hops,
        persist,
        &allowed_headers,
    )
    .await
}

/// Bounded multi-hop redirect chain, address-safety-checked at every hop
/// (scheme/userinfo, host allowlist, DNS resolve-and-pin — the same egress
/// rules on hop 1 and hop N): shared by fetch, refresh, and HTTP search providers
/// so each network verb applies ADR-191 D3's same egress rules.
///
/// `headers_for_hop` is called once per hop with that hop's (possibly
/// redirected) URL; it returns the request headers for that hop and is also
/// the caller's opportunity to re-validate anything URL-dependent before the
/// hop is dialed (`run_fetch` re-checks its credential's host scope there —
/// `run_refresh` never carries a credential, so its closure is a constant).
/// Returns the terminal hop's outcome plus every traversed redirect, in
/// order — never the terminal hop itself, matching [`RedirectHop`]'s doc.
pub(crate) async fn run_hop_chain<F>(
    resolver: &dyn Resolver,
    cfg: &WebSectionConfig,
    url: Url,
    method: reqwest::Method,
    max_bytes: u64,
    deadline: Instant,
    headers_for_hop: F,
) -> Result<(HopOutcome, Vec<RedirectHop>), RuntimeError>
where
    F: FnMut(&Url) -> Result<Vec<(String, String)>, RuntimeError>,
{
    run_hop_chain_with_clients(
        &egress::PinnedClients::default(),
        resolver,
        cfg,
        url,
        method,
        max_bytes,
        deadline,
        headers_for_hop,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_hop_chain_with_clients<F>(
    clients: &egress::PinnedClients,
    resolver: &dyn Resolver,
    cfg: &WebSectionConfig,
    mut url: Url,
    method: reqwest::Method,
    max_bytes: u64,
    deadline: Instant,
    mut headers_for_hop: F,
) -> Result<(HopOutcome, Vec<RedirectHop>), RuntimeError>
where
    F: FnMut(&Url) -> Result<Vec<(String, String)>, RuntimeError>,
{
    let mut redirects = 0u32;
    let mut redirect_hops: Vec<RedirectHop> = Vec::new();

    loop {
        egress::check_scheme_and_userinfo(&url)?;
        egress::check_allowlist(url.host_str().unwrap_or_default(), cfg)?;
        let hop_headers_out = headers_for_hop(&url)?;
        let host = url
            .host_str()
            .ok_or_else(|| RuntimeError::InvalidInput("url has no host".to_string()))?
            .to_string();
        let addr = egress::resolve_and_pin_before(resolver, &host, deadline).await?;
        let client = clients.for_checked_address(&url, addr)?;

        let outcome = run_one_hop(
            &client,
            &url,
            method.clone(),
            &hop_headers_out,
            max_bytes,
            deadline,
        )
        .await?;

        match &outcome.redirect_to {
            Some(next) => {
                egress::check_redirect_cap(redirects, MAX_REDIRECTS)?;
                redirect_hops.push(RedirectHop {
                    from: url.clone(),
                    to: next.clone(),
                    status: outcome.status,
                });
                redirects += 1;
                url = next.clone();
                continue;
            }
            None => return Ok((outcome, redirect_hops)),
        }
    }
}

/// A resource/page identity resolved for one URL, minted (bare, if absent)
/// under its `site`. `site contains {page|resource}` is linked here too so
/// every caller (redirect hop or terminal) gets it uniformly.
pub(crate) async fn mint_bare(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    url: &Url,
) -> Result<(Uuid, Uuid), RuntimeError> {
    let request_url = identity::request_url(url.clone());
    let canonical = identity::canonicalize(request_url.clone());
    let site = canonical_site(runtime, token, &canonical).await?;
    let path_and_query = identity::path_and_query(&canonical);
    let id = identity::document_id(site, &path_and_query);
    crate::entities::get_or_create(
        runtime,
        token,
        id,
        "document",
        "resource",
        canonical.as_ref(),
        json!({ "url": request_url.to_string(), "status": Value::Null }),
    )
    .await?;
    runtime
        .link(token, site, id, EdgeRelation::Contains, 1.0, None)
        .await?;
    Ok((site, id))
}

pub(crate) async fn canonical_site(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    canonical: &Url,
) -> Result<Uuid, RuntimeError> {
    let id = identity::site_id(canonical);
    let (entity, _created) = crate::entities::get_or_create(
        runtime,
        token,
        id,
        "service",
        "site",
        &identity::site_key(canonical),
        json!({
            "scheme": canonical.scheme(),
            "host": canonical.host_str(),
            "port": canonical.port_or_known_default(),
        }),
    )
    .await?;
    Ok(entity.id)
}

/// Persist only permanent redirect endpoints and `new supersedes old` (D2).
/// Temporary hops remain receipt data: neither mint nor patch an entity merely
/// because it participated in such a hop. The caller settles the terminal
/// response separately, whether its preceding redirect was permanent or not.
/// Returns each endpoint touched by permanent hops once.
pub(crate) async fn settle_redirect_hops(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    redirect_hops: &[RedirectHop],
) -> Result<Vec<Uuid>, RuntimeError> {
    let mut entities_touched: Vec<Uuid> = Vec::new();
    for hop in redirect_hops {
        if !matches!(hop.status, 301 | 308) {
            continue;
        }
        let (_from_site, from_id) = mint_bare(runtime, token, &hop.from).await?;
        let (_to_site, to_id) = mint_bare(runtime, token, &hop.to).await?;
        crate::entities::patch(
            runtime,
            token,
            from_id,
            None,
            json!({ "status": hop.status, "redirect_to": hop.to.to_string() }),
        )
        .await?;
        for id in [from_id, to_id] {
            if !entities_touched.contains(&id) {
                entities_touched.push(id);
            }
        }
        runtime
            .link(token, to_id, from_id, EdgeRelation::Supersedes, 1.0, None)
            .await?;
    }
    Ok(entities_touched)
}

/// Root a stored body on the record that holds it. Blob garbage collection
/// keeps a blob alive through the `attachments` table (ADR-121), not through
/// a property that names it, so `blob_ref` alone would leave a page's own
/// body collectable once the grace period passes. Attachments are owned by
/// the canonical main backend, hence `core()`: a pack routed to another
/// backend still roots there.
pub(crate) async fn root_body(
    runtime: &KhiveRuntime,
    record: Uuid,
    substrate: AttachmentSubstrate,
    content_ref: &ContentRef,
    media_type: Option<&str>,
    size_bytes: u64,
) -> Result<(), RuntimeError> {
    let attachment = Attachment::from_new(
        record,
        substrate,
        NewAttachment {
            role: "content".to_string(),
            content_ref: content_ref.clone(),
            media_type: media_type.map(str::to_string),
            size_bytes: Some(size_bytes),
        },
        chrono::Utc::now().timestamp_micros(),
    );
    attachment
        .validate()
        .map_err(|error| RuntimeError::Internal(format!("body attachment invalid: {error}")))?;
    runtime
        .core()
        .attachments()?
        .upsert_attachment(attachment)
        .await
        .map_err(|error| RuntimeError::Internal(format!("body attachment write failed: {error}")))
}

/// Mint (if absent), blob-store the body, and patch one page/resource
/// entity's full row: identity resolve, `site contains {page|resource}`
/// link (arm29: minted before the blob put, so a failing store still leaves
/// a fetchable placeholder behind), blob put, then the fetched-content
/// property patch (url/content_type/blob_ref/content_digest/size/status/
/// fetched_at/etag/last_modified). Shared by [`settle_with_request_headers`] (`web.fetch`'s
/// terminal hop, when `persist` is set) and `ingest::ingest_disk_file`
/// (`web.ingest`'s disk-tree path, which always persists), so there is one
/// row-minting code path for both a fetched and an ingested `page`/
/// `resource` row rather than two independently maintained ones.
pub(crate) struct SettledContent {
    pub id: Uuid,
    pub content_ref: Option<String>,
    pub bytes: u64,
    pub truncated: bool,
}

pub(crate) enum ContentBody {
    Received(Vec<u8>, bool),
    Stored {
        content_ref: ContentRef,
        bytes: u64,
        truncated: bool,
    },
}

pub(crate) fn representation_patch(
    url: &str,
    content_type: Option<&str>,
    status: u16,
    etag: Option<&str>,
    last_modified: Option<&str>,
    content_ref: Option<&str>,
    bytes: u64,
) -> Value {
    json!({
        "url": url,
        "content_type": content_type,
        "blob_ref": content_ref,
        "content_digest": content_ref,
        "size": bytes,
        "status": status,
        "fetched_at": chrono::Utc::now().to_rfc3339(),
        "etag": etag,
        "last_modified": last_modified,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn settle_content(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    url: &Url,
    content_type: Option<&str>,
    status: u16,
    etag: Option<&str>,
    last_modified: Option<&str>,
    body: Option<(Vec<u8>, bool)>,
) -> Result<SettledContent, RuntimeError> {
    settle_content_body(
        runtime,
        token,
        url,
        content_type,
        status,
        etag,
        last_modified,
        body.map(|(bytes, truncated)| ContentBody::Received(bytes, truncated)),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn settle_content_body(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    url: &Url,
    content_type: Option<&str>,
    status: u16,
    etag: Option<&str>,
    last_modified: Option<&str>,
    body: Option<ContentBody>,
) -> Result<SettledContent, RuntimeError> {
    let request_url = identity::request_url(url.clone());
    let canonical = identity::canonicalize(request_url.clone());
    let site = canonical_site(runtime, token, &canonical).await?;
    let path_and_query = identity::path_and_query(&canonical);
    let id = identity::document_id(site, &path_and_query);
    let entity_type = classify_entity_type(content_type);
    let (existing, _) = crate::entities::get_or_create(
        runtime,
        token,
        id,
        "document",
        entity_type,
        canonical.as_ref(),
        json!({ "url": request_url.to_string() }),
    )
    .await?;
    runtime
        .link(token, site, id, EdgeRelation::Contains, 1.0, None)
        .await?;

    // HEAD describes the remote representation, not the bytes already stored.
    // Keep their metadata and validators together until a GET replaces them.
    if body.is_none()
        && existing
            .properties
            .as_ref()
            .and_then(|properties| properties.get("blob_ref"))
            .and_then(Value::as_str)
            .is_some()
    {
        crate::entities::patch(runtime, token, id, None, json!({ "status": status })).await?;
        return Ok(SettledContent {
            id,
            content_ref: None,
            bytes: 0,
            truncated: false,
        });
    }

    let (typed_ref, bytes, truncated) = match body {
        None => (None, 0u64, false),
        Some(ContentBody::Received(buffer, truncated)) => {
            let store = crate::blob_store(runtime)?;
            let len = buffer.len() as u64;
            let content_ref = store.put(buffer).await.map_err(RuntimeError::from)?;
            (Some(content_ref), len, truncated)
        }
        Some(ContentBody::Stored {
            content_ref,
            bytes,
            truncated,
        }) => (Some(content_ref), bytes, truncated),
    };
    let content_ref = typed_ref.as_ref().map(ToString::to_string);

    crate::entities::patch(
        runtime,
        token,
        id,
        Some(entity_type),
        representation_patch(
            request_url.as_ref(),
            content_type,
            status,
            etag,
            last_modified,
            content_ref.as_deref(),
            bytes,
        ),
    )
    .await?;
    if let Some(typed_ref) = &typed_ref {
        root_body(
            runtime,
            id,
            AttachmentSubstrate::Entity,
            typed_ref,
            content_type,
            bytes,
        )
        .await?;
    }

    Ok(SettledContent {
        id,
        content_ref,
        bytes,
        truncated,
    })
}

/// Everything after the redirect loop settles on a final hop: permanent
/// redirects record their endpoints and `new supersedes old`; temporary hops
/// are receipt data only. The terminal hop's GET body (if any) goes to the
/// blob store before the receipt is written (D4), and the receipt/reply
/// share one allow-listed header projection.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn settle_with_request_headers(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    method_name: &str,
    final_url: &Url,
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body: Option<(Vec<u8>, bool)>,
    redirect_hops: &[RedirectHop],
    persist: bool,
    request_headers: &[(String, String)],
) -> Result<Value, RuntimeError> {
    let mut entities_touched: Vec<Uuid> = Vec::new();

    if persist {
        entities_touched.extend(settle_redirect_hops(runtime, token, redirect_hops).await?);
    }

    let response_headers_json = extract_allowed_headers(headers);
    let content_type = header_str(headers, "content-type").map(str::to_string);

    if !persist {
        if let Some((bytes, truncated)) = body.as_ref() {
            let encoded_size = base64::encoded_len(bytes.len(), true);
            if encoded_size.is_none_or(|size| size > INLINE_BODY_BUDGET) {
                return Err(RuntimeError::InvalidInput(
                    "web.fetch: transient base64 body exceeds the inline response budget; lower max_bytes or use persist=true".into(),
                ));
            }
            // An empty body string already includes its JSON quotes. Base64
            // adds no escaping; a canonical receipt UUID has this fixed width.
            let envelope = json!({
                "final_url": final_url.to_string(), "status": status,
                "headers": response_headers_json, "content_ref": null,
                "bytes": bytes.len() as u64, "truncated": truncated,
                "redirects": redirect_hops.len() as u32,
                "receipt_id": Uuid::nil().to_string(), "id": null, "body": "",
            });
            let metadata_size = serde_json::to_vec(&envelope)
                .map_err(|e| RuntimeError::Internal(format!("web.fetch: response metadata: {e}")))?
                .len();
            let fits = encoded_size
                .and_then(|encoded| encoded.checked_add(metadata_size))
                .is_some_and(|total| total <= INLINE_RESULT_BUDGET);
            if !fits {
                return Err(RuntimeError::InvalidInput(
                    "web.fetch: transient base64 body and metadata exceed the inline response budget; lower max_bytes or use persist=true".into(),
                ));
            }
        }
    }

    let fetched_at = chrono::Utc::now().to_rfc3339();
    let mut content_digest = None;
    let mut response_body = None;
    let (final_entity_id, content_ref, bytes, truncated) = if persist {
        let settled = settle_content(
            runtime,
            token,
            final_url,
            content_type.as_deref(),
            status,
            header_str(headers, "etag"),
            header_str(headers, "last-modified"),
            body,
        )
        .await?;
        if !entities_touched.contains(&settled.id) {
            entities_touched.push(settled.id);
        }
        (
            Some(settled.id),
            settled.content_ref,
            settled.bytes,
            settled.truncated,
        )
    } else {
        match body {
            None => (None, None, 0u64, false),
            Some((buffer, truncated)) => {
                let len = buffer.len() as u64;
                content_digest = Some(blake3::hash(&buffer).to_hex().to_string());
                response_body = Some(buffer);
                (None, None, len, truncated)
            }
        }
    };

    let content_digest = content_digest.or_else(|| content_ref.clone());

    // A HEAD cannot replace the request context of a cached GET body.
    if method_name == "GET" {
        if let Some(id) = final_entity_id {
            persist_negotiation_headers(runtime, token, id, request_headers).await?;
        }
    }

    let redirect_chain: Vec<Value> = redirect_hops
        .iter()
        .map(|hop| json!({ "from": hop.from.to_string(), "to": hop.to.to_string(), "status": hop.status }))
        .collect();

    let request_record = json!({
        "verb": "web.fetch",
        "method": method_name,
        "final_url": final_url.to_string(),
        "status": status,
        "headers": response_headers_json,
        "request_headers": negotiation_headers(request_headers),
        "bytes": bytes,
        "truncated": truncated,
        "content_ref": content_ref,
        "content_digest": content_digest,
        "size": bytes,
        "fetched_at": fetched_at,
        "redirects": redirect_hops.len() as u32,
        "redirect_chain": redirect_chain,
    });
    let receipt_id = write_receipt(
        runtime,
        token,
        &format!("web.fetch {method_name} {final_url}"),
        request_record,
        entities_touched,
    )
    .await
    .map_err(|error| {
        RuntimeError::Internal(format!(
            "web.fetch: receipt write failed after body settlement (content_ref={:?}): {error}",
            content_ref
        ))
    })?;
    Ok(json!({
        "final_url": final_url.to_string(),
        "status": status,
        "headers": response_headers_json,
        "content_ref": content_ref,
        "bytes": bytes,
        "truncated": truncated,
        "redirects": redirect_hops.len() as u32,
        "receipt_id": receipt_id.to_string(),
        "id": final_entity_id.map(|id| id.to_string()),
        "body": response_body.as_deref().map(|bytes| BASE64.encode(bytes)),
    }))
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn settle(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    method_name: &str,
    final_url: &Url,
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body: Option<(Vec<u8>, bool)>,
    redirect_hops: &[RedirectHop],
    persist: bool,
) -> Result<Value, RuntimeError> {
    settle_with_request_headers(
        runtime,
        token,
        method_name,
        final_url,
        status,
        headers,
        body,
        redirect_hops,
        persist,
        &[],
    )
    .await
}

impl WebPack {
    pub(crate) async fn handle_fetch(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        self.handle_fetch_with_clients(token, params, &egress::PinnedClients::default())
            .await
    }

    pub(crate) async fn handle_fetch_with_clients(
        &self,
        token: &NamespaceToken,
        params: Value,
        clients: &egress::PinnedClients,
    ) -> Result<Value, RuntimeError> {
        let params: FetchParams = serde_json::from_value(params).map_err(|error| {
            RuntimeError::InvalidInput(format!("invalid web.fetch arguments: {error}"))
        })?;
        let effective_token = resolve_effective_token(token, params.namespace.as_deref())?;
        run_fetch(
            &self.runtime,
            &effective_token,
            &SystemResolver,
            &self.runtime.config().web,
            params,
            clients,
        )
        .await
    }
}

/// Deferred mechanics/receipt/blob/entity tests (A1, A3, A4's fetch half, A6
/// carrying ADR-175 arms 11/12/16/19/21/24/25/28/29/30 forward).
///
/// These stay in-crate rather than in `tests/`: `run_one_hop` and `settle`
/// are `pub(crate)`/private on purpose — this is a published public crate,
/// and neither function has a reason to join the public API just to be
/// test-reachable from an external integration file. A real local
/// `TcpListener` exercises `run_one_hop`/`settle` directly (bypassing
/// `resolve_and_pin`, which would refuse 127.0.0.1 as loopback on every hop,
/// including production's own real requests to a real listener — an
/// accepted limitation, not a gap, matching `egress.rs`'s own precedent).
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use khive_pack_kg::KgPack;
    use khive_runtime::engine_config::WebCredentialConfig;
    use khive_runtime::{Namespace, VerbRegistryBuilder};
    use khive_storage::{
        BlobStore, ContentRef, Direction, EntityFilter, PageRequest, StorageError, StorageResult,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// The in-crate test runtime carries no `VerbRegistry`, so the web
    /// pack's own `EDGE_RULES` (`site contains page|resource`) are never
    /// installed on it by default — `link(EdgeRelation::Contains, ...)`
    /// then refuses every one of these tests against the base allowlist
    /// alone. Register kg+web through a throwaway registry purely to read
    /// back their combined `all_edge_rules()` and install it, matching how
    /// `khive-pack-kg`/`khive-pack-workspace` tests register their own
    /// rules.
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

    #[tokio::test(start_paused = true)]
    async fn dns_stall_is_bounded_by_fetch_total_deadline_before_any_body_is_stored() {
        use crate::egress::resolver_fixture::ScriptedResolver;
        let (runtime, token, dir) = test_runtime().await;
        for phase in [1, 2] {
            let resolver = ScriptedResolver::new(Some(phase));
            let params =
                serde_json::from_value(json!({"url":"https://example.test/", "timeout_s":1}))
                    .unwrap();
            let start = tokio::time::Instant::now();
            let error = tokio::time::timeout(
                Duration::from_secs(2),
                run_fetch(
                    &runtime,
                    &token,
                    &resolver,
                    &WebSectionConfig::default(),
                    params,
                    &egress::PinnedClients::default(),
                ),
            )
            .await
            .expect("fetch must finish within its one-second bound, not the watchdog")
            .unwrap_err();
            assert!(error.to_string().contains("response_too_slow"), "{error}");
            assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(1));
            assert_eq!(resolver.calls.load(Ordering::SeqCst), phase);
            assert!(resolver.cancelled.load(Ordering::SeqCst));
            assert_eq!(count_blob_files(dir.path()), 0);
        }
        // A prompt answer reaches normal address classification rather than timeout.
        let mut resolver = ScriptedResolver::new(None);
        resolver.address = "127.0.0.1".parse().unwrap();
        let params =
            serde_json::from_value(json!({"url":"https://example.test/", "timeout_s":1})).unwrap();
        let error = run_fetch(
            &runtime,
            &token,
            &resolver,
            &WebSectionConfig::default(),
            params,
            &egress::PinnedClients::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("address_loopback"), "{error}");
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn partial_ceiling_config_refuses_programmatic_fetch_before_resolution() {
        use crate::egress::resolver_fixture::ScriptedResolver;
        let (runtime, token, _dir) = test_runtime().await;
        let mut resolver = ScriptedResolver::new(None);
        // The baseline reaches DNS; a loopback answer makes that regression
        // fail without opening an external connection.
        resolver.address = "127.0.0.1".parse().unwrap();
        for config in [
            WebSectionConfig {
                timeout_max_s: Some(1),
                ..Default::default()
            },
            WebSectionConfig {
                max_bytes_max: Some(1),
                ..Default::default()
            },
            WebSectionConfig {
                timeout_max_s: Some(u64::MAX),
                ..Default::default()
            },
        ] {
            let params = serde_json::from_value(json!({"url":"https://example.test/"})).unwrap();
            let error = run_fetch(
                &runtime,
                &token,
                &resolver,
                &config,
                params,
                &egress::PinnedClients::default(),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("invalid_web_config"), "{error}");
            assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        }
    }

    /// Counts stored blob objects only — skips the store's own root
    /// write-lock file (`.khive-blob-write.lock`, created on the FIRST
    /// `put` and left behind for the life of the store; not a blob). A count
    /// that includes that lock file reads 2 where it should read 1 after
    /// exactly one `put`.
    fn count_blob_files(path: &std::path::Path) -> usize {
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    count += count_blob_files(&p);
                } else if p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| !n.starts_with('.'))
                    .unwrap_or(true)
                {
                    count += 1;
                }
            }
        }
        count
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

    fn http_head_response(
        status: u16,
        reason: &str,
        extra_headers: &[(&str, String)],
        content_length: u64,
    ) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
        for (name, value) in extra_headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.extend_from_slice(format!("Content-Length: {content_length}\r\n").as_bytes());
        out.extend_from_slice(b"Connection: close\r\n\r\n");
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

    /// Serve `responses` in order across sequential connections on ONE
    /// listener (one bind, one port): unlike `spawn_once`, a caller can dial
    /// the SAME address twice and get two different canned responses — the
    /// shape the repeat-fetch test below needs to hit the identical url on
    /// both requests.
    async fn spawn_sequence(responses: Vec<Vec<u8>>) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = hits.clone();
        tokio::spawn(async move {
            for response in responses {
                if let Ok((mut stream, _)) = listener.accept().await {
                    hits_task.fetch_add(1, Ordering::SeqCst);
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                }
            }
        });
        (port, hits)
    }

    async fn spawn_once_delayed(
        response: Vec<u8>,
        delay: std::time::Duration,
    ) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = hits.clone();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                hits_task.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                tokio::time::sleep(delay).await;
                let _ = stream.write_all(&response).await;
                let _ = stream.shutdown().await;
            }
        });
        (port, hits)
    }

    fn plain_client(timeout: std::time::Duration) -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .gzip(true)
            .build()
            .expect("plain client builds")
    }

    fn local_url(port: u16, path: &str) -> Url {
        Url::parse(&format!("http://127.0.0.1:{port}{path}")).expect("valid local url")
    }

    async fn entity_count(runtime: &KhiveRuntime, token: &NamespaceToken) -> usize {
        runtime
            .entities(token)
            .expect("entity store capability")
            .query_entities(
                "local",
                EntityFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: 1_000,
                },
            )
            .await
            .expect("query entities")
            .items
            .len()
    }

    async fn run_hop_and_settle(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        method: reqwest::Method,
        url: &Url,
        max_bytes: u64,
    ) -> (HopOutcome, Value) {
        let client = plain_client(Duration::from_secs(5));
        let outcome = run_one_hop(
            &client,
            url,
            method.clone(),
            &[],
            max_bytes,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("hop succeeds");
        let method_name = method.to_string();
        let reply = settle(
            runtime,
            token,
            &method_name,
            &outcome.final_url,
            outcome.status,
            &outcome.headers,
            outcome.body.clone(),
            &[],
            true,
        )
        .await
        .expect("settle");
        (outcome, reply)
    }

    // A1 (+ former arm 11/16/30): fetch mints site+page/resource+blob+receipt;
    // an identical second fetch of the SAME address (same listener, second
    // request) returns the same blob reference, mints no new entity, and
    // writes a receipt only; a different body is the control that yields a
    // different reference.
    // A1.2: receipts never own the fetched body. Must fail if receipt rooting
    // returns or entity rooting is removed. Control: a fresh HEAD roots nothing.
    #[tokio::test]
    async fn fetched_body_is_rooted_only_on_entity_head_roots_nothing() {
        let (runtime, token, _dir) = test_runtime().await;
        let url = Url::parse("https://rooted.example.test/page.html").unwrap();
        let body = b"<html><body>rooted body</body></html>".to_vec();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("content-type", "text/html".parse().unwrap());
        let reply = settle(
            &runtime,
            &token,
            "GET",
            &url,
            200,
            &headers,
            Some((body.clone(), false)),
            &[],
            true,
        )
        .await
        .expect("settle persists");
        let entity_id = Uuid::parse_str(reply["id"].as_str().unwrap()).unwrap();
        let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
        let content_ref = reply["content_ref"].as_str().unwrap().to_string();

        let attachments = runtime.attachments().expect("attachment store");
        let on_entity = attachments
            .list_attachments(entity_id)
            .await
            .expect("list entity attachments");
        assert_eq!(on_entity.len(), 1, "one content attachment on the page");
        assert_eq!(on_entity[0].role, "content");
        assert_eq!(on_entity[0].content_ref.to_string(), content_ref);
        assert_eq!(on_entity[0].size_bytes, Some(body.len() as u64));
        let on_receipt = attachments
            .list_attachments(receipt_id)
            .await
            .expect("list receipt attachments");
        assert!(
            on_receipt.is_empty(),
            "the receipt never roots a fetched body"
        );

        let head_url = Url::parse("https://rooted.example.test/other.html").unwrap();
        let head_reply = settle(
            &runtime,
            &token,
            "HEAD",
            &head_url,
            200,
            &headers,
            None,
            &[],
            true,
        )
        .await
        .expect("HEAD settles");
        let head_entity = Uuid::parse_str(head_reply["id"].as_str().unwrap()).unwrap();
        let head_receipt = Uuid::parse_str(head_reply["receipt_id"].as_str().unwrap()).unwrap();
        assert!(attachments
            .list_attachments(head_entity)
            .await
            .unwrap()
            .is_empty());
        assert!(attachments
            .list_attachments(head_receipt)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn a1_fetch_mints_entities_repeat_fetch_reuses_blob_writes_receipt_only() {
        let (runtime, token, dir) = test_runtime().await;
        let body = b"<html><body>hi</body></html>".to_vec();
        let response = http_response(
            200,
            "OK",
            &[("Content-Type", "text/html".to_string())],
            &body,
        );
        let (port, hits) = spawn_sequence(vec![response.clone(), response]).await;
        let url = local_url(port, "/page");
        let (_outcome, reply) =
            run_hop_and_settle(&runtime, &token, reqwest::Method::GET, &url, 10_000).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let id = reply["id"].as_str().expect("entity id").to_string();
        let content_ref = reply["content_ref"].as_str().unwrap().to_string();
        assert_eq!(count_blob_files(dir.path()), 1);

        let entity = runtime
            .entities(&token)
            .unwrap()
            .get_entity(uuid::Uuid::parse_str(&id).unwrap())
            .await
            .unwrap()
            .expect("entity persisted");
        assert_eq!(entity.entity_type.as_deref(), Some("page"));
        let props = entity.properties.unwrap();
        assert_eq!(props["blob_ref"], content_ref);

        // Repeat fetch of the identical url, against the SAME listener/
        // address: same blob ref, no new object, no new entity, still
        // exactly one receipt-bearing note beyond the first.
        let before_notes = runtime
            .list_notes(&token, Some("observation"), 100, 0)
            .await
            .unwrap()
            .len();
        let before_entities = entity_count(&runtime, &token).await;
        let (_outcome2, reply2) =
            run_hop_and_settle(&runtime, &token, reqwest::Method::GET, &url, 10_000).await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "the same address was hit twice"
        );
        assert_eq!(
            reply2["id"], id,
            "the repeat resolves to the same entity, by address"
        );
        assert_eq!(reply2["content_ref"], content_ref);
        assert_eq!(count_blob_files(dir.path()), 1, "no new object stored");
        let after_entities = entity_count(&runtime, &token).await;
        assert_eq!(
            after_entities, before_entities,
            "no new entity minted by the repeat"
        );
        let after_notes = runtime
            .list_notes(&token, Some("observation"), 100, 0)
            .await
            .unwrap()
            .len();
        assert_eq!(after_notes, before_notes + 1, "exactly one new receipt");

        // Control: a different body, at a different address, yields a
        // different reference.
        let (port3, _hits3) = spawn_once(http_response(200, "OK", &[], b"different body")).await;
        let url3 = local_url(port3, "/other");
        let (_outcome3, reply3) =
            run_hop_and_settle(&runtime, &token, reqwest::Method::GET, &url3, 10_000).await;
        assert_ne!(reply3["content_ref"], content_ref);
    }

    // A3: a 301 chain yields `new supersedes old`; a 302 yields no edge and
    // a receipt naming the hop. Exercised directly against `settle` with a
    // synthetic redirect_hops slice (the transport half of following actual
    // redirects is `run_fetch`'s loop, covered structurally by arm21/arm24
    // below; this arm is the one that owns the entity/edge assertions).
    #[tokio::test]
    async fn a3_permanent_redirect_supersedes_temporary_redirect_no_edge() {
        let (runtime, token, _dir) = test_runtime().await;
        let old = Url::parse("https://old.example.test/moved").unwrap();
        let new = Url::parse("https://new.example.test/here").unwrap();
        let hop = RedirectHop {
            from: old.clone(),
            to: new.clone(),
            status: 301,
        };
        let reply = settle(
            &runtime,
            &token,
            "GET",
            &new,
            200,
            &reqwest::header::HeaderMap::new(),
            Some((b"landed".to_vec(), false)),
            &[hop],
            true,
        )
        .await
        .expect("settle");
        let new_id = uuid::Uuid::parse_str(reply["id"].as_str().unwrap()).unwrap();
        let old_id = identity::document_id(
            identity::site_id(&identity::canonicalize(old.clone())),
            &identity::path_and_query(&identity::canonicalize(old)),
        );
        let neighbors = runtime
            .neighbors(&token, new_id, Direction::Out, None, None)
            .await
            .unwrap();
        assert!(
            neighbors
                .iter()
                .any(|n| n.node_id == old_id && n.relation == EdgeRelation::Supersedes),
            "301 chain must yield new supersedes old"
        );

        // Control: a 302 hop yields no supersedes edge, just a receipt
        // naming the hop.
        let temp_old = Url::parse("https://temp.example.test/a").unwrap();
        let temp_new = Url::parse("https://temp.example.test/b").unwrap();
        let temp_hop = RedirectHop {
            from: temp_old.clone(),
            to: temp_new.clone(),
            status: 302,
        };
        let reply2 = settle(
            &runtime,
            &token,
            "GET",
            &temp_new,
            200,
            &reqwest::header::HeaderMap::new(),
            Some((b"landed2".to_vec(), false)),
            &[temp_hop],
            true,
        )
        .await
        .expect("settle");
        let temp_new_id = uuid::Uuid::parse_str(reply2["id"].as_str().unwrap()).unwrap();
        let temp_old_id = identity::document_id(
            identity::site_id(&identity::canonicalize(temp_old.clone())),
            &identity::path_and_query(&identity::canonicalize(temp_old)),
        );
        let neighbors2 = runtime
            .neighbors(&token, temp_new_id, Direction::Out, None, None)
            .await
            .unwrap();
        assert!(
            !neighbors2.iter().any(|n| n.node_id == temp_old_id),
            "302 must not yield an edge"
        );
        let receipt_id = uuid::Uuid::parse_str(reply2["receipt_id"].as_str().unwrap()).unwrap();
        let note = runtime
            .notes(&token)
            .unwrap()
            .get_note(receipt_id)
            .await
            .unwrap()
            .unwrap();
        let chain = note.properties.unwrap()["request"]["redirect_chain"].clone();
        assert_eq!(chain[0]["status"], 302, "the hop is named in the receipt");
    }

    // arm 11: a response larger than the byte bound stores truncated, and
    // the stored bytes are exactly the first `max_bytes` bytes of the
    // original body. A within-bound response is the positive control.
    #[tokio::test]
    async fn arm11_over_byte_bound_stores_truncated_prefix() {
        let full_body = vec![b'x'; 100];
        let max_bytes = 40u64;
        let response = http_response(200, "OK", &[], &full_body);
        let (port, hits) = spawn_once(response).await;
        let url = local_url(port, "/big");
        let client = plain_client(Duration::from_secs(5));
        let outcome = run_one_hop(
            &client,
            &url,
            reqwest::Method::GET,
            &[],
            max_bytes,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("hop succeeds");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(outcome.status, 200);
        let (buffer, truncated) = outcome.body.clone().expect("GET carries a body slot");
        assert!(truncated);
        assert_eq!(buffer.len() as u64, max_bytes);
        assert_eq!(buffer, full_body[..max_bytes as usize]);

        let (runtime, token, _dir) = test_runtime().await;
        let reply = settle(
            &runtime,
            &token,
            "GET",
            &outcome.final_url,
            outcome.status,
            &outcome.headers,
            outcome.body.clone(),
            &[],
            true,
        )
        .await
        .expect("settle stores + records receipt");
        assert_eq!(reply["truncated"], true);
        assert_eq!(reply["bytes"], max_bytes);
        let content_ref = reply["content_ref"]
            .as_str()
            .expect("content_ref")
            .to_string();

        // Positive control: within-bound is not truncated.
        let small_body = vec![b'y'; 10];
        let (port2, _hits2) = spawn_once(http_response(200, "OK", &[], &small_body)).await;
        let url2 = local_url(port2, "/small");
        let outcome2 = run_one_hop(
            &client,
            &url2,
            reqwest::Method::GET,
            &[],
            max_bytes,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("hop succeeds");
        let (buffer2, truncated2) = outcome2.body.expect("GET carries a body slot");
        assert!(!truncated2);
        assert_eq!(buffer2, small_body);

        let store = crate::blob_store(&runtime).unwrap();
        let content_ref_parsed = ContentRef::from_hex(&content_ref).expect("valid content ref hex");
        let stored = store
            .get_bounded_verified(&content_ref_parsed, khive_storage::MAX_BLOB_WHOLE_BYTES)
            .await
            .expect("stored bytes read back and digest-verified");
        assert_eq!(stored, buffer);
    }

    // arm 12: a response slower than the time bound refuses, and the blob
    // store holds nothing new; a within-bound response is the control.
    #[tokio::test]
    async fn arm12_over_time_bound_refuses_blob_store_holds_nothing_within_bound_succeeds() {
        let (_runtime, _token, dir) = test_runtime().await;
        assert_eq!(count_blob_files(dir.path()), 0, "blob dir starts empty");

        let body = b"too slow".to_vec();
        let (port, hits) = spawn_once_delayed(
            http_response(200, "OK", &[], &body),
            Duration::from_millis(300),
        )
        .await;
        let url = local_url(port, "/slow");
        let client = plain_client(Duration::from_secs(10));
        let err = run_one_hop(
            &client,
            &url,
            reqwest::Method::GET,
            &[],
            1_000,
            Instant::now() + Duration::from_millis(50),
        )
        .await
        .expect_err("a hop past the deadline refuses");
        let message = err.to_string();
        assert!(message.contains("response_too_slow"), "{message}");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the connection was made");
        assert_eq!(
            count_blob_files(dir.path()),
            0,
            "no object stored on a timed-out hop"
        );

        let (port2, _hits2) = spawn_once(http_response(200, "OK", &[], b"fast")).await;
        let url2 = local_url(port2, "/fast");
        let outcome = run_one_hop(
            &client,
            &url2,
            reqwest::Method::GET,
            &[],
            1_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("within-bound hop succeeds");
        assert_eq!(outcome.status, 200);
    }

    // A4: refresh's unchanged-etag no-op is exercised in refresh.rs; this is
    // the fetch-side half: a HEAD response advertising a nonzero
    // content-length yields no content_ref/blob write, and the receipt
    // headers match the reply's exactly with an unlisted header dropped
    // from both (former arm 28). A GET control reads and stores.
    #[tokio::test]
    async fn a4_head_response_yields_no_content_ref_get_control_stores() {
        let (runtime, token, dir) = test_runtime().await;
        assert_eq!(count_blob_files(dir.path()), 0);

        let head_headers = [
            ("Content-Type", "text/plain".to_string()),
            ("X-Unlisted", "should-not-appear".to_string()),
        ];
        let (port, hits) = spawn_once(http_head_response(200, "OK", &head_headers, 42)).await;
        let url = local_url(port, "/head");
        let client = plain_client(Duration::from_secs(5));
        let outcome = run_one_hop(
            &client,
            &url,
            reqwest::Method::HEAD,
            &[],
            1_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("HEAD hop succeeds");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(outcome.body.is_none(), "HEAD never carries a body slot");

        let reply = settle(
            &runtime,
            &token,
            "HEAD",
            &outcome.final_url,
            outcome.status,
            &outcome.headers,
            outcome.body.clone(),
            &[],
            true,
        )
        .await
        .expect("settle");
        assert_eq!(reply["content_ref"], Value::Null);
        assert_eq!(reply["bytes"], 0);
        assert_eq!(reply["truncated"], false);
        assert_eq!(
            count_blob_files(dir.path()),
            0,
            "no blob put for a HEAD response"
        );

        let headers = reply["headers"].as_object().unwrap();
        assert_eq!(headers.get("content-type").unwrap(), "text/plain");
        assert!(
            !headers.contains_key("x-unlisted"),
            "unlisted header omitted from the reply"
        );

        let receipt_id = uuid::Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
        let note = runtime
            .notes(&token)
            .unwrap()
            .get_note(receipt_id)
            .await
            .unwrap()
            .unwrap();
        let properties = note.properties.unwrap();
        let record = properties.get("request").unwrap();
        assert_eq!(
            record["headers"], reply["headers"],
            "receipt headers match the reply exactly"
        );

        // GET control: reads and stores its body.
        let get_body = b"actual bytes".to_vec();
        let (port2, _hits2) = spawn_once(http_response(200, "OK", &[], &get_body)).await;
        let url2 = local_url(port2, "/get");
        let outcome2 = run_one_hop(
            &client,
            &url2,
            reqwest::Method::GET,
            &[],
            1_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("GET hop succeeds");
        let reply2 = settle(
            &runtime,
            &token,
            "GET",
            &outcome2.final_url,
            outcome2.status,
            &outcome2.headers,
            outcome2.body.clone(),
            &[],
            true,
        )
        .await
        .expect("settle");
        assert!(reply2["content_ref"].is_string());
        assert_eq!(reply2["bytes"], get_body.len() as u64);
        assert_eq!(
            count_blob_files(dir.path()),
            1,
            "the GET control stores exactly one new object"
        );
    }

    // arm 19: a gzip response whose decompressed size exceeds the byte
    // bound stores truncated at exactly the bound.
    #[tokio::test]
    async fn arm19_gzip_response_truncates_after_decompression_to_the_bound() {
        let plaintext = vec![b'z'; 10_000];
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        {
            use std::io::Write as _;
            encoder.write_all(&plaintext).unwrap();
        }
        let compressed = encoder.finish().unwrap();
        assert!(
            compressed.len() < plaintext.len(),
            "fixture must actually compress"
        );

        let max_bytes = 100u64;
        let response = http_response(
            200,
            "OK",
            &[("Content-Encoding", "gzip".to_string())],
            &compressed,
        );
        let (port, _hits) = spawn_once(response).await;
        let url = local_url(port, "/gz");
        let client = plain_client(Duration::from_secs(5));
        let outcome = run_one_hop(
            &client,
            &url,
            reqwest::Method::GET,
            &[],
            max_bytes,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("hop succeeds");
        let (buffer, truncated) = outcome.body.clone().expect("GET body");
        assert!(
            truncated,
            "decompressed body exceeds max_bytes and must truncate"
        );
        assert_eq!(buffer.len() as u64, max_bytes);
        assert!(buffer.iter().all(|&b| b == b'z'));

        let (runtime, token, _dir) = test_runtime().await;
        let reply = settle(
            &runtime,
            &token,
            "GET",
            &outcome.final_url,
            outcome.status,
            &outcome.headers,
            Some((buffer.clone(), truncated)),
            &[],
            true,
        )
        .await
        .expect("settle stores the truncated decompressed prefix");
        assert_eq!(reply["truncated"], true);
        assert_eq!(reply["bytes"], max_bytes);
        let content_ref = reply["content_ref"].as_str().unwrap().to_string();
        let store = crate::blob_store(&runtime).unwrap();
        let content_ref_parsed = ContentRef::from_hex(&content_ref).unwrap();
        let size = store
            .size(&content_ref_parsed)
            .await
            .unwrap()
            .expect("object exists");
        assert!(
            size <= max_bytes,
            "stored object must be no larger than the bound"
        );
        assert_eq!(size, max_bytes);
    }

    // arm 21: a redirect whose second hop is outside the credential's host
    // set refuses at that hop — hop 1 is made, hop 2 is not.
    #[tokio::test]
    async fn arm21_redirect_second_hop_outside_credential_set_refuses_hop2_never_dialed() {
        let (hop1_port, hop1_hits) = spawn_once(http_response(
            302,
            "Found",
            &[("Location", "https://elsewhere.test/next".to_string())],
            b"",
        ))
        .await;
        let (_hop2_port, hop2_hits) =
            spawn_once(http_response(200, "OK", &[], b"never reached")).await;

        let url = local_url(hop1_port, "/start");
        let client = plain_client(Duration::from_secs(5));
        let outcome = run_one_hop(
            &client,
            &url,
            reqwest::Method::GET,
            &[],
            1_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("hop 1 executes");
        assert_eq!(hop1_hits.load(Ordering::SeqCst), 1, "hop 1 was made");
        assert_eq!(outcome.status, 302);
        let redirect_to = outcome
            .redirect_to
            .expect("a Location header yields a redirect target");
        assert_eq!(redirect_to.host_str(), Some("elsewhere.test"));

        let mut cfg = WebSectionConfig::default();
        cfg.credentials.push(WebCredentialConfig {
            name: "token".to_string(),
            env_var: "UNUSED_TEST_VAR".to_string(),
            hosts: vec!["127.0.0.1".to_string()],
        });
        let err =
            egress::check_credential(&cfg, "token", redirect_to.host_str().unwrap()).unwrap_err();
        assert_eq!(err.code, "credential_host_mismatch");
        assert_eq!(hop2_hits.load(Ordering::SeqCst), 0, "hop 2 was not made");

        let mut cfg2 = WebSectionConfig::default();
        cfg2.credentials.push(WebCredentialConfig {
            name: "token".to_string(),
            env_var: "UNUSED_TEST_VAR".to_string(),
            hosts: vec!["elsewhere.test".to_string()],
        });
        assert!(egress::check_credential(&cfg2, "token", redirect_to.host_str().unwrap()).is_ok());
    }

    // arm 24 (redirect half): a redirect to a URL carrying userinfo refuses
    // before the next hop is ever requested.
    #[tokio::test]
    async fn arm24_redirect_to_userinfo_url_refuses_before_next_hop_is_dialed() {
        let (hop1_port, hop1_hits) = spawn_once(http_response(
            302,
            "Found",
            &[("Location", "https://user:pass@elsewhere.test/x".to_string())],
            b"",
        ))
        .await;
        let (_hop2_port, hop2_hits) =
            spawn_once(http_response(200, "OK", &[], b"never reached")).await;

        let url = local_url(hop1_port, "/start");
        let client = plain_client(Duration::from_secs(5));
        let outcome = run_one_hop(
            &client,
            &url,
            reqwest::Method::GET,
            &[],
            1_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("hop 1 executes");
        assert_eq!(hop1_hits.load(Ordering::SeqCst), 1);
        let redirect_to = outcome
            .redirect_to
            .expect("a Location header yields a redirect target");
        assert_eq!(redirect_to.username(), "user");

        let err = egress::check_scheme_and_userinfo(&redirect_to).unwrap_err();
        assert_eq!(err.code, "userinfo_present");
        assert_eq!(
            hop2_hits.load(Ordering::SeqCst),
            0,
            "the second hop is never requested"
        );

        let mut stripped = redirect_to.clone();
        stripped.set_username("").unwrap();
        stripped.set_password(None).unwrap();
        assert!(egress::check_scheme_and_userinfo(&stripped).is_ok());
    }

    // arm 25 (dispatch half): max_bytes/timeout_s ceilings refuse when
    // wired through the real `handle_fetch` dispatch path.
    #[tokio::test]
    async fn arm25_dispatch_ceiling_refusals_wired_through_handle_fetch() {
        let (runtime, token, _dir) = test_runtime().await;
        let pack = WebPack::new(runtime.clone());

        let over_bytes = pack
            .handle_fetch(
                &token,
                json!({
                    "url": "https://example.test/",
                    "max_bytes": khive_runtime::engine_config::WebCeilings::default().max_bytes_max + 1,
                }),
            )
            .await
            .unwrap_err();
        assert!(
            over_bytes.to_string().contains("ceiling_exceeded"),
            "{over_bytes}"
        );

        let over_timeout = pack
            .handle_fetch(
                &token,
                json!({
                    "url": "https://example.test/",
                    "timeout_s": khive_runtime::engine_config::WebCeilings::default().timeout_max_s + 1,
                }),
            )
            .await
            .unwrap_err();
        assert!(
            over_timeout.to_string().contains("ceiling_exceeded"),
            "{over_timeout}"
        );

        let at_ceiling = pack
            .handle_fetch(
                &token,
                json!({
                    "url": "ftp://example.test/",
                    "max_bytes": khive_runtime::engine_config::WebCeilings::default().max_bytes_max,
                    "timeout_s": khive_runtime::engine_config::WebCeilings::default().timeout_max_s,
                }),
            )
            .await
            .unwrap_err();
        assert!(
            !at_ceiling.to_string().contains("ceiling_exceeded"),
            "{at_ceiling}"
        );
        assert!(
            at_ceiling.to_string().contains("scheme_not_allowed"),
            "{at_ceiling}"
        );
    }

    #[derive(Debug)]
    struct FailingPutBlobStore {
        put_calls: AtomicUsize,
    }

    impl FailingPutBlobStore {
        fn new() -> Self {
            Self {
                put_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl BlobStore for FailingPutBlobStore {
        async fn put(&self, _bytes: Vec<u8>) -> StorageResult<ContentRef> {
            self.put_calls.fetch_add(1, Ordering::SeqCst);
            Err(StorageError::Transaction {
                operation: "put".into(),
                message: "simulated put failure".to_string(),
            })
        }
        async fn get_bounded_verified(
            &self,
            _content_ref: &ContentRef,
            _max_bytes: u64,
        ) -> StorageResult<Vec<u8>> {
            Err(StorageError::Transaction {
                operation: "get_bounded_verified".into(),
                message: "not used by this test double".to_string(),
            })
        }
        async fn exists(&self, _content_ref: &ContentRef) -> StorageResult<bool> {
            Ok(false)
        }
        async fn size(&self, _content_ref: &ContentRef) -> StorageResult<Option<u64>> {
            Ok(None)
        }
        async fn delete(&self, _content_ref: &ContentRef) -> StorageResult<bool> {
            Err(StorageError::Transaction {
                operation: "delete".into(),
                message: "not used by this test double".to_string(),
            })
        }
    }

    // arm 29: a put failure prevents the receipt from ever being written,
    // and the transport hop is attempted exactly once (no retry) — but the
    // entity rows (minted before the blob put) persist regardless, since
    // identity is by address and independent of any one fetch's success.
    #[tokio::test]
    async fn arm29_put_failure_prevents_receipt_entities_still_minted_no_retry() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        install_web_edge_rules(&runtime);
        let failing_store = Arc::new(FailingPutBlobStore::new());
        runtime
            .install_blob_store(failing_store.clone() as Arc<dyn BlobStore>)
            .expect("install failing store");
        let token = runtime.authorize(Namespace::local()).expect("authorize");
        let before = runtime
            .list_notes(&token, Some("observation"), 100, 0)
            .await
            .unwrap()
            .len();

        let body = b"never stored".to_vec();
        let (port, hits) = spawn_once(http_response(200, "OK", &[], &body)).await;
        let url = local_url(port, "/fail");
        let client = plain_client(Duration::from_secs(5));
        let outcome = run_one_hop(
            &client,
            &url,
            reqwest::Method::GET,
            &[],
            1_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("the transport hop itself succeeds");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let err = settle(
            &runtime,
            &token,
            "GET",
            &outcome.final_url,
            outcome.status,
            &outcome.headers,
            outcome.body.clone(),
            &[],
            true,
        )
        .await
        .expect_err("a failing store refuses settle before any receipt write");
        let message = err.to_string();
        assert!(
            message.contains("simulated put failure") || message.contains("storage"),
            "{message}"
        );
        assert_eq!(
            failing_store.put_calls.load(Ordering::SeqCst),
            1,
            "put attempted exactly once"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "no retry of the transport hop"
        );
        let after = runtime
            .list_notes(&token, Some("observation"), 100, 0)
            .await
            .unwrap()
            .len();
        assert_eq!(after, before, "a failed put left no receipt behind");

        let canonical = identity::canonicalize(outcome.final_url.clone());
        let site = identity::site_id(&canonical);
        let id = identity::document_id(site, &identity::path_and_query(&canonical));
        assert!(
            runtime
                .entities(&token)
                .unwrap()
                .get_entity(id)
                .await
                .unwrap()
                .is_some(),
            "the entity row was minted before the failing put"
        );
    }

    // arm 30: a fetched object's put goes through the same content-addressed
    // BlobStore contract an ordinary blob.put would use — a local read by
    // content_ref succeeds independent of receipt outcome.
    #[tokio::test]
    async fn arm30_fetch_put_matches_direct_put_content_addressing() {
        let (runtime, token, _dir) = test_runtime().await;
        let store = crate::blob_store(&runtime).unwrap();

        let payload = b"identical bytes via either path".to_vec();
        let direct_ref = store.put(payload.clone()).await.expect("direct put");

        let (port, _hits) = spawn_once(http_response(200, "OK", &[], &payload)).await;
        let url = local_url(port, "/same-bytes");
        let (_outcome, reply) =
            run_hop_and_settle(&runtime, &token, reqwest::Method::GET, &url, 10_000).await;
        let via_fetch_ref = reply["content_ref"].as_str().unwrap();
        assert_eq!(
            via_fetch_ref,
            direct_ref.to_string(),
            "identical bytes content-address identically through either path (idempotent put, ADR-111)"
        );

        let read_back = store
            .get_bounded_verified(&direct_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
            .await
            .expect("read by content_ref alone, independent of receipt outcome");
        assert_eq!(read_back, payload);
    }

    #[test]
    fn classify_entity_type_html_variants_are_page_everything_else_is_resource() {
        assert_eq!(classify_entity_type(Some("text/html")), "page");
        assert_eq!(
            classify_entity_type(Some("text/html; charset=utf-8")),
            "page"
        );
        assert_eq!(classify_entity_type(Some("APPLICATION/XHTML+XML")), "page");
        assert_eq!(classify_entity_type(Some("application/json")), "resource");
        assert_eq!(classify_entity_type(Some("text/plain")), "resource");
        assert_eq!(classify_entity_type(None), "resource");
    }

    // `accept` becomes the Accept request header, through the exact same
    // allow-list `headers` goes through; an explicit headers["Accept"]
    // alongside it refuses (naming the conflict) rather than one silently
    // winning. Control: accept alone, beside an unrelated allowed header,
    // succeeds and both are present.
    #[test]
    fn accept_param_becomes_accept_header_conflicting_with_headers_refuses() {
        let empty = BTreeMap::new();
        let out = effective_request_headers(&empty, Some("application/json")).unwrap();
        assert_eq!(
            out,
            vec![("Accept".to_string(), "application/json".to_string())]
        );

        let mut conflicting = BTreeMap::new();
        conflicting.insert("Accept".to_string(), "text/plain".to_string());
        let err = effective_request_headers(&conflicting, Some("application/json")).unwrap_err();
        assert_eq!(err.code, "header_conflict");

        let mut with_other = BTreeMap::new();
        with_other.insert("User-Agent".to_string(), "khive".to_string());
        let ok = effective_request_headers(&with_other, Some("text/html")).unwrap();
        assert_eq!(ok.len(), 2);
        assert!(ok.contains(&("User-Agent".to_string(), "khive".to_string())));
        assert!(ok.contains(&("Accept".to_string(), "text/html".to_string())));
    }

    // The header `effective_request_headers` computes for `accept` actually
    // reaches the wire — read back from the raw bytes a real local listener
    // received, via `run_one_hop` directly (same unguarded pattern every
    // other mechanics test in this module uses).
    #[tokio::test]
    async fn accept_param_header_reaches_the_wire() {
        let hop_headers =
            effective_request_headers(&BTreeMap::new(), Some("application/vnd.khive+json"))
                .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().unwrap().port();
        let received: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let received_task = received.clone();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                if let Ok(n) = stream.read(&mut buf).await {
                    received_task.lock().unwrap().extend_from_slice(&buf[..n]);
                }
                let _ = stream.write_all(&http_response(200, "OK", &[], b"")).await;
                let _ = stream.shutdown().await;
            }
        });

        let client = plain_client(Duration::from_secs(5));
        let url = local_url(port, "/x");
        run_one_hop(
            &client,
            &url,
            reqwest::Method::GET,
            &hop_headers,
            1_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("hop succeeds");

        let raw = received.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
        assert!(
            text.contains("accept: application/vnd.khive+json"),
            "{text}"
        );
    }

    // The redirect hop cap refuses once already at the cap (the
    // `max_redirects`+1-th redirect), naming the cap in the message; every
    // count below the cap is allowed — the boundary is the whole guard, so
    // it is checked at every step from 0 up to and including the cap.
    #[test]
    fn redirect_cap_refuses_at_cap_allows_below() {
        for redirects in 0..MAX_REDIRECTS {
            assert!(
                egress::check_redirect_cap(redirects, MAX_REDIRECTS).is_ok(),
                "redirects={redirects} must still be allowed"
            );
        }
        let err = egress::check_redirect_cap(MAX_REDIRECTS, MAX_REDIRECTS).unwrap_err();
        assert_eq!(err.code, "redirect_limit_exceeded");
        assert!(
            err.message.contains(&MAX_REDIRECTS.to_string()),
            "{}",
            err.message
        );
    }

    // Only GET and HEAD are permitted methods. The check runs before
    // any address is even parsed, so a refusal needs no network; GET/HEAD
    // pass the method gate specifically and proceed to fail for the
    // unrelated, deterministic reason that a loopback address always
    // refuses (proof that method_not_allowed did NOT fire for them).
    #[tokio::test]
    async fn post_refuses_get_and_head_pass_the_method_check() {
        let (runtime, token, _dir) = test_runtime().await;
        let pack = WebPack::new(runtime.clone());

        let post = pack
            .handle_fetch(
                &token,
                json!({"url": "http://127.0.0.1:9/x", "method": "POST"}),
            )
            .await
            .unwrap_err();
        assert!(post.to_string().contains("method_not_allowed"), "{post}");

        for method in ["GET", "HEAD"] {
            let err = pack
                .handle_fetch(
                    &token,
                    json!({"url": "http://127.0.0.1:9/x", "method": method}),
                )
                .await
                .unwrap_err();
            let msg = err.to_string();
            assert!(!msg.contains("method_not_allowed"), "{method}: {msg}");
            assert!(
                msg.contains("address_loopback"),
                "{method}: expected the method check to pass and the address check to be \
                 the one that refused: {msg}"
            );
        }
    }

    // Only the allow-listed response headers are ever surfaced — a
    // disallowed header is dropped even though it was actually present in
    // the response, both from the reply AND from what the receipt stores;
    // an allow-listed header alongside it is kept in both places.
    #[tokio::test]
    async fn disallowed_response_header_not_persisted_allowed_header_is() {
        let (runtime, token, _dir) = test_runtime().await;
        let body = b"hi".to_vec();
        let response = http_response(
            200,
            "OK",
            &[
                ("Content-Type", "text/plain".to_string()),
                ("X-Powered-By", "leaked".to_string()),
            ],
            &body,
        );
        let (port, _hits) = spawn_once(response).await;
        let url = local_url(port, "/x");
        let (_outcome, reply) =
            run_hop_and_settle(&runtime, &token, reqwest::Method::GET, &url, 10_000).await;

        let headers = reply["headers"].as_object().unwrap();
        assert_eq!(headers.get("content-type").unwrap(), "text/plain");
        assert!(
            !headers.contains_key("x-powered-by"),
            "disallowed response header must not reach the reply"
        );

        let receipt_id = uuid::Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
        let note = runtime
            .notes(&token)
            .unwrap()
            .get_note(receipt_id)
            .await
            .unwrap()
            .unwrap();
        let properties = note.properties.unwrap();
        let request = properties.get("request").unwrap();
        let stored_headers = request["headers"].as_object().unwrap();
        assert_eq!(stored_headers.get("content-type").unwrap(), "text/plain");
        assert!(
            !stored_headers.contains_key("x-powered-by"),
            "disallowed response header must not reach the receipt either"
        );
    }
    #[tokio::test]
    async fn head_preserves_a_previously_fetched_body_and_its_attachment() {
        let (runtime, token, dir) = test_runtime().await;
        let url = Url::parse("https://head.example.test/page").unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("content-type", "text/html".parse().unwrap());
        headers.insert("etag", "\"body-v1\"".parse().unwrap());
        let fetched = settle(
            &runtime,
            &token,
            "GET",
            &url,
            200,
            &headers,
            Some((b"<p>stored body</p>".to_vec(), false)),
            &[],
            true,
        )
        .await
        .unwrap();
        let id = Uuid::parse_str(fetched["id"].as_str().unwrap()).unwrap();
        let before = runtime
            .entities(&token)
            .unwrap()
            .get_entity(id)
            .await
            .unwrap()
            .unwrap();
        let attachments = runtime
            .attachments()
            .unwrap()
            .list_attachments(id)
            .await
            .unwrap();
        let blobs_before = count_blob_files(dir.path());

        let mut head_headers = reqwest::header::HeaderMap::new();
        head_headers.insert("content-type", "application/octet-stream".parse().unwrap());
        head_headers.insert("etag", "\"remote-v2\"".parse().unwrap());
        let head = settle(
            &runtime,
            &token,
            "HEAD",
            &url,
            204,
            &head_headers,
            None,
            &[],
            true,
        )
        .await
        .unwrap();
        assert_eq!(head["id"], fetched["id"]);
        assert!(head["content_ref"].is_null());
        assert_eq!(head["bytes"], 0);
        let after = runtime
            .entities(&token)
            .unwrap()
            .get_entity(id)
            .await
            .unwrap()
            .unwrap();
        let mut expected_properties = before.properties.unwrap();
        expected_properties["status"] = json!(204);
        assert_eq!(after.properties.as_ref().unwrap(), &expected_properties);
        assert_eq!(after.entity_type, before.entity_type);
        let after_attachments = runtime
            .attachments()
            .unwrap()
            .list_attachments(id)
            .await
            .unwrap();
        assert_eq!(after_attachments.len(), attachments.len());
        assert_eq!(after_attachments[0].content_ref, attachments[0].content_ref);
        assert_eq!(after_attachments[0].size_bytes, attachments[0].size_bytes);
        assert_eq!(count_blob_files(dir.path()), blobs_before);
        let head_receipt = Uuid::parse_str(head["receipt_id"].as_str().unwrap()).unwrap();
        assert!(runtime
            .attachments()
            .unwrap()
            .list_attachments(head_receipt)
            .await
            .unwrap()
            .is_empty());

        let empty_get = settle(
            &runtime,
            &token,
            "GET",
            &url,
            200,
            &head_headers,
            Some((Vec::new(), false)),
            &[],
            true,
        )
        .await
        .unwrap();
        assert_ne!(empty_get["content_ref"], fetched["content_ref"]);
        let emptied = runtime
            .entities(&token)
            .unwrap()
            .get_entity(id)
            .await
            .unwrap()
            .unwrap();
        let properties = emptied.properties.unwrap();
        assert_eq!(properties["size"], 0);
        assert_eq!(properties["blob_ref"], empty_get["content_ref"]);
        assert_eq!(emptied.entity_type.as_deref(), Some("resource"));
    }
}

#[cfg(test)]
#[path = "fetch_r2_tests.rs"]
mod r2_tests;

#[cfg(test)]
mod connection_reuse_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn same_origin_hops_reuse_one_tcp_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Exactly one accept: a second client would fail to complete the
        // second request before its deadline, instead of passing a cache-size assertion.
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut paths = Vec::new();
            for _ in 0..2 {
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                }
                paths.push(
                    String::from_utf8(request)
                        .unwrap()
                        .lines()
                        .next()
                        .unwrap()
                        .to_owned(),
                );
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                    )
                    .await
                    .unwrap();
            }
            paths
        });
        let clients = egress::PinnedClients::default();
        // Mechanical transport test, as in run_one_hop tests: production's
        // resolver rejects loopback before consulting this cache.
        for path in ["/first", "/second"] {
            let url = Url::parse(&format!("http://127.0.0.1:{port}{path}")).unwrap();
            let client = clients
                .for_checked_address(&url, "127.0.0.1".parse().unwrap())
                .unwrap();
            let outcome = run_one_hop(
                &client,
                &url,
                reqwest::Method::GET,
                &[],
                1024,
                Instant::now() + std::time::Duration::from_secs(3),
            )
            .await
            .unwrap();
            assert_eq!(outcome.body.unwrap().0, b"ok");
        }
        let paths = tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(paths, ["GET /first HTTP/1.1", "GET /second HTTP/1.1"]);
    }

    #[tokio::test]
    async fn cached_client_does_not_skip_new_address_policy_or_header_checks() {
        use crate::egress::resolver_fixture::ScriptedResolver;
        let clients = egress::PinnedClients::default();
        let url = Url::parse("https://example.test/resource").unwrap();
        clients
            .for_checked_address(&url, "93.184.216.34".parse().unwrap())
            .unwrap();
        let mut resolver = ScriptedResolver::new(None);
        resolver.address = "127.0.0.1".parse().unwrap();
        let error = run_hop_chain_with_clients(
            &clients,
            &resolver,
            &WebSectionConfig::default(),
            url.clone(),
            reqwest::Method::GET,
            1024,
            Instant::now() + std::time::Duration::from_secs(1),
            |_| Ok(vec![]),
        )
        .await
        .err()
        .expect("cached client must not bypass policy refusal");
        assert!(error.to_string().contains("address_loopback"), "{error}");
        let error = run_hop_chain_with_clients(
            &clients,
            &resolver,
            &WebSectionConfig::default(),
            url,
            reqwest::Method::GET,
            1024,
            Instant::now() + std::time::Duration::from_secs(1),
            |_| Err(Refusal::new("credential_host_mismatch", "changed credential scope").into()),
        )
        .await
        .err()
        .expect("cached client must not bypass policy refusal");
        assert!(
            error.to_string().contains("credential_host_mismatch"),
            "{error}"
        );
    }
}
