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
use std::future::Future;
use std::time::Instant;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use khive_runtime::engine_config::WebSectionConfig;
use khive_runtime::{EntityPatch, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{EdgeRelation, Entity};
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
const ALLOWED_RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "last-modified",
    "etag",
    "vary",
    "content-language",
];

/// Preserve every Vary field line. A non-UTF-8 line becomes JSON null rather
/// than disappearing: refresh must treat that stored selector as unreplayable.
pub(crate) fn vary_value(headers: &reqwest::header::HeaderMap) -> Option<Value> {
    let values: Vec<Value> = headers
        .get_all("vary")
        .iter()
        .map(|value| value.to_str().map_or(Value::Null, |text| json!(text)))
        .collect();
    (!values.is_empty()).then_some(Value::Array(values))
}

/// Content-Language is a list field; combine repeated valid lines in order.
/// An invalid supplied value clears the interpretable cached language rather
/// than silently retaining an older language from a different response.
pub(crate) fn content_language_value(headers: &reqwest::header::HeaderMap) -> Option<Value> {
    let values: Vec<_> = headers.get_all("content-language").iter().collect();
    if values.is_empty() {
        return None;
    }
    let mut text = Vec::with_capacity(values.len());
    for value in values {
        let Ok(value) = value.to_str() else {
            return Some(Value::Null);
        };
        text.push(value.trim());
    }
    Some(json!(text.join(", ")))
}

pub(crate) fn extract_allowed_headers(headers: &reqwest::header::HeaderMap) -> Value {
    let mut out = serde_json::Map::new();
    for name in ALLOWED_RESPONSE_HEADERS {
        if let Some(value) = headers.get(*name) {
            if let Ok(text) = value.to_str() {
                out.insert((*name).to_string(), Value::String(text.to_string()));
            }
        }
    }
    if let Some(vary) = vary_value(headers) {
        out.insert("vary".to_string(), vary);
    }
    if let Some(language) = content_language_value(headers) {
        out.insert("content-language".to_string(), language);
    }
    let links: Vec<&str> = headers
        .get_all("link")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    if !links.is_empty() {
        out.insert("link".to_string(), json!(links));
    }
    Value::Object(out)
}

/// The fetch path never offers a content coding and never decodes one: every
/// body it hands on is the identity representation or the request is refused.
pub(crate) const FIXED_ACCEPT_ENCODING: &str = "identity";
/// The value earlier versions recorded when the client offered gzip. A stored
/// request map carrying it is valid for negotiation replay, but its body must
/// be replaced by an unconditional identity GET before validators can be sent.
pub(crate) const LEGACY_ACCEPT_ENCODING: &str = "gzip";
const NEGOTIATION_HEADERS: &[&str] = &["accept", "accept-language", "accept-encoding"];

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

/// `run_one_hop` sends this header on every hop. The egress allowlist does not
/// permit callers to set Accept-Encoding. Record that fixed client choice
/// alongside the fields passed explicitly to `run_one_hop`.
pub(crate) fn recorded_negotiation_headers(
    headers: &[(String, String)],
) -> BTreeMap<String, Vec<String>> {
    let mut selected = negotiation_headers(headers);
    selected.insert(
        "accept-encoding".to_string(),
        vec![FIXED_ACCEPT_ENCODING.to_string()],
    );
    selected
}

pub(crate) fn stored_negotiation_headers(
    properties: &Value,
) -> Result<Vec<(String, String)>, RuntimeError> {
    let stored = properties
        .get("request_headers")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            RuntimeError::InvalidInput(
                "stored request negotiation is missing or invalid".to_string(),
            )
        })?;
    if stored
        .keys()
        .any(|name| !NEGOTIATION_HEADERS.contains(&name.as_str()))
    {
        return Err(RuntimeError::InvalidInput(
            "stored request negotiation contains an unknown header".to_string(),
        ));
    }
    let mut headers = Vec::new();
    for name in NEGOTIATION_HEADERS {
        if let Some(value) = stored.get(*name) {
            let values: Vec<String> = serde_json::from_value(value.clone()).map_err(|error| {
                RuntimeError::InvalidInput(format!("stored {name} negotiation is invalid: {error}"))
            })?;
            if values.is_empty()
                || values.iter().any(|value| {
                    value.trim().is_empty()
                        || reqwest::header::HeaderValue::from_str(value).is_err()
                })
            {
                return Err(RuntimeError::InvalidInput(format!(
                    "stored {name} negotiation has no valid header value"
                )));
            }
            if *name == "accept-encoding" {
                if values.len() != 1
                    || (values[0] != FIXED_ACCEPT_ENCODING && values[0] != LEGACY_ACCEPT_ENCODING)
                {
                    return Err(RuntimeError::InvalidInput(
                        "stored accept-encoding differs from the fixed client value".to_string(),
                    ));
                }
                headers.push(((*name).to_string(), FIXED_ACCEPT_ENCODING.to_string()));
                continue;
            }
            headers.extend(values.into_iter().map(|value| ((*name).to_string(), value)));
        }
    }
    Ok(headers)
}

/// Bind response selection and request negotiation to this GET body revision.
pub(crate) async fn persist_get_context(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    settled: &SettledContent,
    headers: &[(String, String)],
    response_headers: &reqwest::header::HeaderMap,
) -> Result<(), RuntimeError> {
    persist_selection_context(
        runtime,
        token,
        settled,
        recorded_negotiation_headers(headers),
        response_headers,
    )
    .await
}

/// Disk ingest has no HTTP request, so it must not claim the client's fixed
/// Accept-Encoding was sent. It still binds the observed-empty Vary context
/// to the newly stored body.
pub(crate) async fn persist_disk_context(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    settled: &SettledContent,
) -> Result<(), RuntimeError> {
    persist_selection_context(
        runtime,
        token,
        settled,
        BTreeMap::new(),
        &reqwest::header::HeaderMap::new(),
    )
    .await
}

async fn persist_selection_context(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    settled: &SettledContent,
    selected: BTreeMap<String, Vec<String>>,
    response_headers: &reqwest::header::HeaderMap,
) -> Result<(), RuntimeError> {
    let stored = settled
        .entity
        .properties
        .as_ref()
        .map(stored_negotiation_headers)
        .transpose()
        .ok()
        .flatten()
        .map(|headers| negotiation_headers(&headers));
    let vary = vary_value(response_headers).unwrap_or_else(|| json!([]));
    let content_language = content_language_value(response_headers).unwrap_or(Value::Null);
    let mut patch = serde_json::Map::new();
    if stored != Some(selected.clone()) {
        patch.insert("request_headers".to_string(), json!(selected));
    }
    if settled
        .entity
        .properties
        .as_ref()
        .and_then(|p| p.get("vary"))
        != Some(&vary)
    {
        patch.insert("vary".to_string(), vary);
    }
    if settled
        .entity
        .properties
        .as_ref()
        .and_then(|p| p.get("content_language"))
        != Some(&content_language)
    {
        patch.insert("content_language".to_string(), content_language);
    }
    if !patch.is_empty() {
        runtime
            .update_entity_if_unchanged(
                token,
                &settled.entity,
                EntityPatch {
                    properties: Some(Value::Object(patch)),
                    ..Default::default()
                },
                &[],
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
    // The requested representation is fixed for every hop, including HEAD and
    // redirects: identity, sent explicitly so no origin is invited to compress.
    let mut request = client
        .request(method, url.clone())
        .header(reqwest::header::ACCEPT_ENCODING, FIXED_ACCEPT_ENCODING);
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("accept-encoding") {
            if value != FIXED_ACCEPT_ENCODING {
                return Err(RuntimeError::InvalidInput(
                    "request accept-encoding differs from the fixed client value".to_string(),
                ));
            }
            continue;
        }
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
                // No client decodes a content coding, so a body that names one
                // would reach the byte cap as undecoded bytes. Refuse it
                // before any body byte is read. A 204 or 304 has no content
                // to mislabel.
                if !matches!(status, 204 | 304) {
                    refuse_content_encoding(&response_headers)?;
                }
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

/// Refuse any `Content-Encoding` other than `identity`. Every listed coding
/// must be identity; an unreadable value is refused as well.
pub(crate) fn refuse_content_encoding(
    headers: &reqwest::header::HeaderMap,
) -> Result<(), RuntimeError> {
    for value in headers.get_all(reqwest::header::CONTENT_ENCODING) {
        let text = value.to_str().ok();
        let coding = text.unwrap_or("<non-text value>");
        let identity_only = text.is_some_and(|text| {
            text.split(',')
                .map(str::trim)
                .all(|token| token.is_empty() || token.eq_ignore_ascii_case("identity"))
        });
        if !identity_only {
            return Err(Refusal::new(
                "unsupported_content_encoding",
                format!(
                    "the response declares content-encoding {coding:?}; only identity is accepted"
                ),
            )
            .into());
        }
    }
    Ok(())
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
            .into());
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
    url: Url,
    method: reqwest::Method,
    max_bytes: u64,
    deadline: Instant,
    headers_for_hop: F,
) -> Result<(HopOutcome, Vec<RedirectHop>), RuntimeError>
where
    F: FnMut(&Url) -> Result<Vec<(String, String)>, RuntimeError>,
{
    let (outcome, redirect_hops, ()) = run_hop_chain_with_clients_observed(
        clients,
        resolver,
        cfg,
        url,
        method,
        max_bytes,
        deadline,
        headers_for_hop,
        |_| async { Ok(()) },
    )
    .await?;
    Ok((outcome, redirect_hops))
}

/// Observe each hop immediately before sending it. The returned observation
/// belongs to the terminal request, including when that request followed a
/// redirect; callers can use it as an optimistic-write guard at settlement.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_hop_chain_with_clients_observed<F, O, Fut, T>(
    clients: &egress::PinnedClients,
    resolver: &dyn Resolver,
    cfg: &WebSectionConfig,
    mut url: Url,
    method: reqwest::Method,
    max_bytes: u64,
    deadline: Instant,
    mut headers_for_hop: F,
    mut before_request: O,
) -> Result<(HopOutcome, Vec<RedirectHop>, T), RuntimeError>
where
    F: FnMut(&Url) -> Result<Vec<(String, String)>, RuntimeError>,
    O: FnMut(Url) -> Fut,
    Fut: Future<Output = Result<T, RuntimeError>>,
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
        let request_observation = before_request(url.clone()).await?;

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
            None => return Ok((outcome, redirect_hops, request_observation)),
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
    let id = identity::site_id(token.namespace(), canonical);
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

/// An entity's `content_ref` is projected from the graph backend's local
/// attachment table. The body root may instead live on canonical main, so a
/// snapshot returned before `root_body` cannot be forged into the one that a
/// guarded metadata update will read. Keep the graph row's own revision and
/// body reference stable, then use its actual projection for that guard.
pub(crate) async fn entity_after_body_root(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    patched: &khive_storage::Entity,
    content_ref: &str,
) -> Result<khive_storage::Entity, RuntimeError> {
    let conflict = || {
        RuntimeError::Khive(khive_types::KhiveError::conflict(
            "web body row changed while its content attachment was rooted",
        ))
    };
    let current = runtime
        .entities(token)?
        .get_entity(patched.id)
        .await?
        .ok_or_else(conflict)?;
    if current.namespace != patched.namespace
        || current.deleted_at.is_some()
        || current.version != patched.version
        || current
            .properties
            .as_ref()
            .and_then(|properties| properties.get("blob_ref"))
            .and_then(Value::as_str)
            != Some(content_ref)
    {
        return Err(conflict());
    }
    Ok(current)
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
    pub(crate) entity: Entity,
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
        // Unknown until a GET's exact response Vary is bound to this body.
        // A stale or failed metadata patch cannot make validators replayable.
        "vary": [null],
        "content_language": null,
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
        let entity =
            crate::entities::patch(runtime, token, id, None, json!({ "status": status })).await?;
        return Ok(SettledContent {
            id,
            content_ref: None,
            bytes: 0,
            truncated: false,
            entity,
        });
    }

    let no_body = body.is_none();
    let (typed_ref, bytes, truncated) = match body {
        None => (None, 0u64, false),
        Some((buffer, truncated)) => {
            let store = runtime.require_blob_store()?;
            let len = buffer.len() as u64;
            let content_ref = store.put(buffer).await.map_err(RuntimeError::from)?;
            (Some(content_ref), len, truncated)
        }
    };
    let content_ref = typed_ref.as_ref().map(ToString::to_string);

    let mut properties = representation_patch(
        request_url.as_ref(),
        content_type,
        status,
        etag,
        last_modified,
        content_ref.as_deref(),
        bytes,
    );
    properties["truncated"] = json!(truncated);
    if no_body {
        // HEAD did not observe a representation length. Null also clears a
        // legacy HEAD row that incorrectly reported an empty body as size 0.
        properties["size"] = Value::Null;
    }
    let mut entity =
        crate::entities::patch(runtime, token, id, Some(entity_type), properties).await?;
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
        entity = entity_after_body_root(runtime, token, &entity, typed_ref.as_str()).await?;
    }

    Ok(SettledContent {
        id,
        content_ref,
        bytes,
        truncated,
        entity,
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
        // HEAD cannot replace the request context of a cached GET body.
        if method_name == "GET" {
            persist_get_context(runtime, token, &settled, request_headers, headers).await?;
        }
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

    let redirect_chain: Vec<Value> = redirect_hops
        .iter()
        .map(|hop| json!({ "from": hop.from.to_string(), "to": hop.to.to_string(), "status": hop.status }))
        .collect();

    let mut request_record = json!({
        "verb": "web.fetch",
        "method": method_name,
        "final_url": final_url.to_string(),
        "status": status,
        "headers": response_headers_json,
        "request_headers": recorded_negotiation_headers(request_headers),
        "bytes": bytes,
        "truncated": truncated,
        "content_ref": content_ref,
        "fetched_at": fetched_at,
        "redirects": redirect_hops.len() as u32,
        "redirect_chain": redirect_chain,
    });
    if method_name == "GET" {
        // A GET measures the body even when persist=false leaves the graph
        // untouched. A HEAD has no bytes from which to infer either field.
        request_record["content_digest"] = json!(content_digest);
        request_record["size"] = json!(bytes);
        if content_ref.is_some() {
            request_record["body_entity_id"] = json!(final_entity_id);
        }
    }
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
    if let (Some(id), Some(reference)) = (final_entity_id, content_ref.as_deref()) {
        crate::receipt::bind_capture_receipt(runtime, token, id, reference, receipt_id).await?;
    }
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
mod tests;

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
