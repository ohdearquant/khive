//! `web.search` (ADR-191 D3).
//!
//! Search hits are never entities by themselves — D3 only mints rows when
//! `persist=true`, and even then a hit becomes exactly what `web.fetch`
//! would mint for the same URL before fetching it: an unfetched `resource`
//! under its `site`, `status: null`. A later `web.fetch`/`web.refresh` of
//! that same URL re-types and fills it in place, same as any other
//! extract-discovered link (D1's "identity is by address").

use std::time::Instant;

use khive_runtime::engine_config::{WebSearchProviderConfig, WebSectionConfig};
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::egress::{self, Refusal, Resolver, SystemResolver};
use crate::fetch::{mint_bare, resolve_effective_token, run_one_hop};
use crate::receipt::write_receipt;
use crate::WebPack;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchParams {
    query: String,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    persist: Option<bool>,
    #[serde(default)]
    max_bytes: Option<u64>,
    #[serde(default)]
    timeout_s: Option<u64>,
    #[serde(default)]
    namespace: Option<String>,
}

/// One transcribed result. Deserialized straight off an `Http` provider's
/// response body — intentionally *not* `deny_unknown_fields`, since a
/// third-party provider's response shape is theirs to extend, not ours to
/// police (D6: transcribe, never invent — extra fields are simply dropped).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SearchHit {
    title: String,
    url: String,
    snippet: String,
}

/// Resolve which configured provider serves this call. `name` narrows to
/// one named entry (refusing by name when absent); omitted selects the
/// operator's `default = true` entry, or the sole entry when exactly one is
/// configured. An empty or ambiguous selection refuses with
/// `no_search_provider_configured` rather than an empty result list.
fn select_provider<'a>(
    cfg: &'a WebSectionConfig,
    name: Option<&str>,
) -> Result<&'a WebSearchProviderConfig, RuntimeError> {
    match name {
        Some(name) => cfg
            .search_providers
            .iter()
            .find(|provider| provider.name() == name)
            .ok_or_else(|| {
                RuntimeError::from(Refusal::new(
                    "search_provider_not_configured",
                    format!("search provider {name:?} is not configured"),
                ))
            }),
        None => cfg
            .search_providers
            .iter()
            .find(|provider| provider.is_default())
            .or_else(|| {
                if cfg.search_providers.len() == 1 {
                    cfg.search_providers.first()
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                RuntimeError::from(Refusal::new(
                    "no_search_provider_configured",
                    "no search provider is configured; add a [[web.search_providers]] entry",
                ))
            }),
    }
}

/// Parse a provider's raw response bytes into the transcribed hit list.
/// Pure — no networking, so it is directly unit-testable against a
/// hand-built `(body, truncated)` pair without a live connection.
///
/// A1.3: a byte-bound-exceeding provider response refuses outright — a
/// truncated payload is never treated as a complete result list, unlike
/// web.fetch's store-what-was-read behavior.
fn parse_search_response(
    body: &[u8],
    truncated: bool,
    limit: u32,
) -> Result<Vec<SearchHit>, RuntimeError> {
    if truncated {
        return Err(Refusal::new(
            "response_too_large",
            "search provider response exceeded the configured byte bound",
        )
        .into());
    }
    let raw: Vec<SearchHit> = serde_json::from_slice(body).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "search provider response is not a JSON array of {{title, url, snippet}}: {error}"
        ))
    })?;
    Ok(raw.into_iter().take(limit as usize).collect())
}

/// An `Http` provider's request goes through the same address-class/
/// DNS-rebinding-safe egress checks as `web.fetch`/`web.refresh`
/// (`egress::resolve_and_pin_before` + `egress::pinned_client`) rather than a bare
/// `reqwest::Client` dialing whatever `url_template` names — a provider
/// pointed at a loopback/private/link-local address by misconfiguration (or
/// a rewritten template) is refused before any connection is attempted, the
/// same as it would be for `web.fetch`.
///
/// `api_key_env`'s value is gated behind TWO checks, both evaluated BEFORE
/// any DNS resolution or dial: the resolved URL's host must be inside
/// `provider_hosts` (mirroring `[[web.credentials]].hosts`' scoping), and the
/// URL must be `https`. A provider with no configured `hosts` therefore never
/// attaches its key to any host at all — `WebSectionConfig::validate`
/// enforces that `hosts` is non-empty whenever `api_key_env` is set, so this
/// is a defense in depth, not the only gate.
///
/// DNS resolution and `run_one_hop` share the same absolute deadline. The
/// client carries no independent timeout that could race it and surface a
/// `transport_error` instead of the deliberate `response_too_slow` refusal.
#[allow(clippy::too_many_arguments)]
async fn run_http_provider(
    resolver: &dyn Resolver,
    cfg: &WebSectionConfig,
    url_template: &str,
    api_key_env: Option<&str>,
    provider_hosts: &[String],
    query: &str,
    limit: u32,
    max_bytes: u64,
    deadline: Instant,
) -> Result<Vec<SearchHit>, RuntimeError> {
    let encoded_query: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let url_string = url_template
        .replace("{query}", &encoded_query)
        .replace("{limit}", &limit.to_string());
    let url = url::Url::parse(&url_string).map_err(|error| {
        RuntimeError::InvalidInput(format!("invalid search provider url_template: {error}"))
    })?;
    egress::check_scheme_and_userinfo(&url)?;
    let host = url
        .host_str()
        .ok_or_else(|| RuntimeError::InvalidInput("search provider url has no host".to_string()))?
        .to_string();
    egress::check_allowlist(&host, cfg)?;

    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(env_var) = api_key_env {
        if !egress::host_in_set(provider_hosts, &host) {
            return Err(Refusal::new(
                "search_provider_key_host_mismatch",
                format!("search provider's api_key_env is not scoped to host {host:?}"),
            )
            .into());
        }
        egress::check_credential_scheme(&url)?;
        let value = std::env::var(env_var).map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "search provider api_key_env {env_var:?} is not set"
            ))
        })?;
        headers.push(("Authorization".to_string(), format!("Bearer {value}")));
    }

    let port = url.port_or_known_default().ok_or_else(|| {
        RuntimeError::InvalidInput("search provider url has no resolvable port".to_string())
    })?;
    let addr = egress::resolve_and_pin_before(resolver, &host, deadline).await?;
    let client = egress::pinned_client(&host, addr, port)?;

    let outcome = run_one_hop(
        &client,
        &url,
        reqwest::Method::GET,
        &headers,
        max_bytes,
        deadline,
    )
    .await?;
    let (body, truncated) = outcome.body.ok_or_else(|| {
        RuntimeError::from(Refusal::new(
            "transport_error",
            "search provider returned no readable body",
        ))
    })?;
    parse_search_response(&body, truncated, limit)
}

async fn run_search(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    cfg: &WebSectionConfig,
    params: SearchParams,
) -> Result<Value, RuntimeError> {
    let ceilings = egress::resolve_ceilings(cfg)?;
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
    let limit = egress::check_limit_ceiling(
        params.limit,
        ceilings.search_limit_default,
        ceilings.search_limit_max,
    )?;
    let persist = params.persist.unwrap_or(false);

    let provider = select_provider(cfg, params.provider.as_deref())?;
    let provider_name = provider.name().to_string();
    let deadline = egress::request_deadline(timeout_s)?;

    let hits: Vec<SearchHit> = match provider {
        WebSearchProviderConfig::Fixture { results, .. } => results
            .iter()
            .take(limit as usize)
            .map(|result| SearchHit {
                title: result.title.clone(),
                url: result.url.clone(),
                snippet: result.snippet.clone(),
            })
            .collect(),
        WebSearchProviderConfig::Http {
            url_template,
            api_key_env,
            hosts,
            ..
        } => {
            run_http_provider(
                &SystemResolver,
                cfg,
                url_template,
                api_key_env.as_deref(),
                hosts,
                &params.query,
                limit,
                max_bytes,
                deadline,
            )
            .await?
        }
    };

    let mut annotates: Vec<Uuid> = Vec::new();
    if persist {
        for hit in &hits {
            if let Ok(url) = url::Url::parse(&hit.url) {
                let (_site, id) = mint_bare(runtime, token, &url).await?;
                annotates.push(id);
            }
        }
    }

    let results_value = serde_json::to_value(&hits).map_err(|error| {
        RuntimeError::Internal(format!("web.search: result serialization failed: {error}"))
    })?;
    let results_bytes = serde_json::to_vec(&results_value).map_err(|error| {
        RuntimeError::Internal(format!("web.search: result serialization failed: {error}"))
    })?;
    let digest = blake3::hash(&results_bytes).to_hex().to_string();

    let request_record = json!({
        "verb": "web.search",
        "query": params.query,
        "provider": provider_name,
        "limit": limit,
        "persist": persist,
        "results": results_value,
        "digest": digest,
    });
    let receipt_id = write_receipt(
        runtime,
        token,
        &format!("web.search {:?} via {provider_name}", params.query),
        request_record,
        annotates,
    )
    .await
    .map_err(|error| {
        RuntimeError::Internal(format!("web.search: receipt write failed: {error}"))
    })?;

    Ok(json!({
        "provider": provider_name,
        "results": results_value,
        "receipt_id": receipt_id.to_string(),
    }))
}

impl WebPack {
    pub(crate) async fn handle_search(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let params: SearchParams = serde_json::from_value(params).map_err(|error| {
            RuntimeError::InvalidInput(format!("invalid web.search arguments: {error}"))
        })?;
        let effective_token = resolve_effective_token(token, params.namespace.as_deref())?;
        run_search(
            &self.runtime,
            &effective_token,
            &self.runtime.config().web,
            params,
        )
        .await
    }
}

/// Deferred search tests (A1's search half, A9's provider-bound arms carried
/// forward as arm15/25/26/27/29). Stays in-crate for the same reason as
/// `fetch.rs`'s `tests` module: `run_search`, `select_provider`,
/// `run_http_provider` and `SearchParams` are private to this module.
#[cfg(test)]
mod tests {
    use super::*;
    use khive_pack_kg::KgPack;
    use khive_runtime::engine_config::WebFixtureResult;
    use khive_runtime::VerbRegistryBuilder;
    use khive_storage::EdgeRelation;
    use khive_types::Namespace;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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

    fn params(
        query: &str,
        limit: Option<u32>,
        provider: Option<&str>,
        persist: bool,
    ) -> SearchParams {
        SearchParams {
            query: query.to_string(),
            limit,
            provider: provider.map(|p| p.to_string()),
            persist: Some(persist),
            max_bytes: None,
            timeout_s: None,
            namespace: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn dns_stall_is_bounded_by_search_total_deadline() {
        use crate::egress::resolver_fixture::ScriptedResolver;
        let cfg = WebSectionConfig::default();
        for phase in [1, 2] {
            let resolver = ScriptedResolver::new(Some(phase));
            let start = tokio::time::Instant::now();
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                run_http_provider(
                    &resolver,
                    &cfg,
                    "https://search.example.test/?q={query}",
                    None,
                    &[],
                    "q",
                    1,
                    100,
                    (start + Duration::from_secs(1)).into_std(),
                ),
            )
            .await
            .expect("provider must finish within its deadline, not the watchdog");
            let error = result.unwrap_err();
            assert!(error.to_string().contains("response_too_slow"), "{error}");
            assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(1));
            assert_eq!(resolver.calls.load(Ordering::SeqCst), phase);
            assert!(resolver.cancelled.load(Ordering::SeqCst));
        }
        let mut resolver = ScriptedResolver::new(None);
        resolver.address = "127.0.0.1".parse().unwrap();
        let error = run_http_provider(
            &resolver,
            &cfg,
            "https://search.example.test/?q={query}",
            None,
            &[],
            "q",
            1,
            100,
            (tokio::time::Instant::now() + Duration::from_secs(1)).into_std(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("address_loopback"), "{error}");
    }

    #[tokio::test]
    async fn partial_ceiling_config_refuses_programmatic_search_before_provider_selection() {
        let (runtime, token, _dir) = test_runtime().await;
        for cfg in [
            WebSectionConfig {
                search_limit_max: Some(1),
                ..Default::default()
            },
            WebSectionConfig {
                timeout_max_s: Some(1),
                ..Default::default()
            },
        ] {
            let error = run_search(&runtime, &token, &cfg, params("q", None, None, false))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("invalid_web_config"), "{error}");
        }
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

    // arm 15: no configured provider refuses naming
    // `no_search_provider_configured` rather than an empty result list; a
    // single configured Fixture is selected even unnamed; multiple
    // non-default providers stay ambiguous and refuse the same way.
    #[test]
    fn arm15_no_provider_configured_refuses_single_fixture_succeeds_multiple_stays_ambiguous() {
        let cfg = WebSectionConfig::default();
        let err = select_provider(&cfg, None).unwrap_err();
        assert!(err.to_string().contains("no_search_provider_configured"));

        let mut with_provider = WebSectionConfig::default();
        with_provider
            .search_providers
            .push(WebSearchProviderConfig::Fixture {
                name: "local".to_string(),
                default: false,
                results: vec![],
            });
        assert!(select_provider(&with_provider, None).is_ok());

        let mut ambiguous = WebSectionConfig::default();
        ambiguous
            .search_providers
            .push(WebSearchProviderConfig::Fixture {
                name: "a".to_string(),
                default: false,
                results: vec![],
            });
        ambiguous
            .search_providers
            .push(WebSearchProviderConfig::Fixture {
                name: "b".to_string(),
                default: false,
                results: vec![],
            });
        assert!(select_provider(&ambiguous, None).is_err());
    }

    // arm 25 (dispatch half, search): limit ceiling refuses through the real
    // `handle_search` dispatch path; an at-ceiling call passes parameter
    // validation and fails later for the unrelated, deterministic reason
    // that this runtime configures no provider at all.
    #[tokio::test]
    async fn arm25_dispatch_search_limit_ceiling_refusal_wired_through_handle_search() {
        let (runtime, token, _dir) = test_runtime().await;
        let pack = WebPack::new(runtime.clone());

        let over_limit = pack
            .handle_search(
                &token,
                json!({
                    "query": "anything",
                    "limit": khive_runtime::engine_config::WebCeilings::default().search_limit_max + 1,
                }),
            )
            .await
            .unwrap_err();
        assert!(
            over_limit.to_string().contains("ceiling_exceeded"),
            "{over_limit}"
        );

        let at_ceiling = pack
            .handle_search(
                &token,
                json!({
                    "query": "anything",
                    "limit": khive_runtime::engine_config::WebCeilings::default().search_limit_max,
                }),
            )
            .await
            .unwrap_err();
        assert!(
            !at_ceiling.to_string().contains("ceiling_exceeded"),
            "{at_ceiling}"
        );
        assert!(
            at_ceiling
                .to_string()
                .contains("no_search_provider_configured"),
            "{at_ceiling}"
        );
    }

    /// A bare, unpinned client — matches `fetch::tests`' own precedent for
    /// local-listener tests that need to bypass `egress::resolve_and_pin`
    /// entirely (see that module's test-level doc comment): a fake resolver
    /// answer and a real successful loopback connect cannot coexist, because
    /// `resolve_and_pin`'s output address is the exact literal socket
    /// `pinned_client` dials. Byte/time-bound behavior lives in
    /// `run_one_hop`/`parse_search_response`, neither of which does any
    /// address-class checking, so exercising them this way is direct testing
    /// of the layer that actually implements the behavior, not a weakened
    /// substitute for `run_http_provider`'s own (address-checked) path.
    fn bare_client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .gzip(true)
            .build()
            .expect("bare client builds")
    }

    // arm 26: a provider response over the byte bound or over the time
    // bound refuses outright — never a partial result list. A within-bound
    // response is the positive control.
    #[tokio::test]
    async fn arm26_provider_over_bytes_and_over_time_refuse_no_partial_results_in_bound_succeeds() {
        let big_body = serde_json::to_vec(&vec![
            SearchHit {
                title: "t".into(),
                url: "https://example.test/a".into(),
                snippet: "x".repeat(500),
            };
            5
        ])
        .unwrap();
        let (port, _hits) = spawn_once(http_response(200, "OK", &[], &big_body)).await;
        let url = url::Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let outcome = run_one_hop(
            &bare_client(),
            &url,
            reqwest::Method::GET,
            &[],
            50,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("hop executes");
        let (body, truncated) = outcome.body.expect("GET carries a body");
        let err = parse_search_response(&body, truncated, 5).unwrap_err();
        assert!(err.to_string().contains("response_too_large"), "{err}");

        let (port2, _hits2) = spawn_once_delayed(
            http_response(200, "OK", &[], b"[]"),
            Duration::from_millis(300),
        )
        .await;
        let url2 = url::Url::parse(&format!("http://127.0.0.1:{port2}/")).unwrap();
        let err2 = run_one_hop(
            &bare_client(),
            &url2,
            reqwest::Method::GET,
            &[],
            10_000,
            Instant::now() + Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(err2.to_string().contains("response_too_slow"), "{err2}");

        let small = serde_json::to_vec(&vec![SearchHit {
            title: "ok".into(),
            url: "https://example.test/ok".into(),
            snippet: "fine".into(),
        }])
        .unwrap();
        let (port3, _hits3) = spawn_once(http_response(200, "OK", &[], &small)).await;
        let url3 = url::Url::parse(&format!("http://127.0.0.1:{port3}/")).unwrap();
        let outcome3 = run_one_hop(
            &bare_client(),
            &url3,
            reqwest::Method::GET,
            &[],
            10_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("hop executes");
        let (body3, truncated3) = outcome3.body.expect("GET carries a body");
        let hits = parse_search_response(&body3, truncated3, 5).expect("in-bound response parses");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "ok");
    }

    // An `Http` provider pointed at a loopback address is refused
    // before any connection is attempted — the same address-class check
    // `web.fetch` applies, now also covering `web.search`'s own network
    // path rather than bypassing it via a bare, unpinned client.
    #[tokio::test]
    async fn http_provider_loopback_address_refuses_before_any_dial() {
        let (port, hits) = spawn_once(http_response(200, "OK", &[], b"[]")).await;
        let url_template = format!("http://127.0.0.1:{port}/?q={{query}}&n={{limit}}");
        let cfg = WebSectionConfig::default();
        let err = run_http_provider(
            &SystemResolver,
            &cfg,
            &url_template,
            None,
            &[],
            "q",
            5,
            10_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("loopback"),
            "loopback address classification refuses before any dial: {err}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the refusal happens before resolve_and_pin ever dials the listener"
        );
    }

    // `api_key_env`'s value never leaves for a host outside the
    // provider's own `hosts` — the mismatch check runs before
    // `egress::resolve_and_pin` is ever called, so no DNS resolution or dial
    // is attempted for the wrong host either. `provider_hosts` empty is the
    // production-shape refusal (`WebSectionConfig::validate` additionally
    // refuses this combination at config-load time; this is the runtime
    // enforcement of the same rule).
    #[tokio::test]
    async fn http_provider_key_never_leaves_for_a_host_outside_provider_hosts() {
        let cfg = WebSectionConfig::default();
        let url_template = "https://not-in-the-allowed-set.example.invalid/?q={query}&n={limit}";
        let err = run_http_provider(
            &SystemResolver,
            &cfg,
            url_template,
            Some("UNUSED_TEST_ENV_VAR_NEVER_READ"),
            &["allowed.example.test".to_string()],
            "q",
            5,
            10_000,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        // A `resolution_failed`/`resolution_unstable` code here would mean
        // `resolve_and_pin` ran against a real, unmocked DNS lookup for a
        // host that was never supposed to be dialed — the host-scope
        // mismatch has to be the FIRST refusal, before any network call.
        assert!(
            err.to_string()
                .contains("search_provider_key_host_mismatch"),
            "{err}"
        );
    }

    // arm 27: the search receipt preserves query/provider/effective-limit
    // and the ordered result array exactly (duplicates and nonalphabetical
    // order survive, including under a limit); the recomputed BLAKE3 digest
    // over the preserved bytes matches the receipt's digest and the reply's
    // own result bytes.
    #[tokio::test]
    async fn arm27_receipt_preserves_query_provider_limit_ordered_results_digest_matches() {
        let (runtime, token, _dir) = test_runtime().await;
        let mut cfg = WebSectionConfig::default();
        cfg.search_providers.push(WebSearchProviderConfig::Fixture {
            name: "canned".to_string(),
            default: true,
            results: vec![
                WebFixtureResult {
                    title: "Zebra".into(),
                    url: "https://z.test/".into(),
                    snippet: "z".into(),
                },
                WebFixtureResult {
                    title: "Apple".into(),
                    url: "https://a.test/".into(),
                    snippet: "a".into(),
                },
                WebFixtureResult {
                    title: "Apple".into(),
                    url: "https://a.test/".into(),
                    snippet: "a".into(),
                },
                WebFixtureResult {
                    title: "Mango".into(),
                    url: "https://m.test/".into(),
                    snippet: "m".into(),
                },
            ],
        });

        let reply = run_search(
            &runtime,
            &token,
            &cfg,
            params("fruit", Some(3), None, false),
        )
        .await
        .expect("search dispatches");
        let results = reply["results"].as_array().expect("results array").clone();
        assert_eq!(
            results.len(),
            3,
            "limited to 3, not deduplicated, not reordered"
        );
        assert_eq!(results[0]["title"], "Zebra");
        assert_eq!(results[1]["title"], "Apple");
        assert_eq!(results[2]["title"], "Apple");
        assert_eq!(
            results[1], results[2],
            "the duplicate survives as a duplicate"
        );

        let receipt_id = reply["receipt_id"].as_str().unwrap().to_string();
        let note_id = uuid::Uuid::parse_str(&receipt_id).unwrap();
        let note = runtime
            .notes(&token)
            .unwrap()
            .get_note(note_id)
            .await
            .unwrap()
            .unwrap();
        let properties = note.properties.unwrap();
        let record = properties.get("request").unwrap();
        assert_eq!(record["query"], "fruit");
        assert_eq!(record["provider"], "canned");
        assert_eq!(record["limit"], 3);
        assert_eq!(
            record["results"], reply["results"],
            "receipt array matches the reply array exactly, same order"
        );

        let recomputed_bytes = serde_json::to_vec(&record["results"]).unwrap();
        let recomputed_digest = blake3::hash(&recomputed_bytes).to_hex().to_string();
        assert_eq!(record["digest"], recomputed_digest);
    }

    // D3 persist half (not an ADR-191 acceptance arm — A9 is the two-backend
    // routing arm, which this pack does not yet implement): persist=false
    // mints nothing; persist=true mints each hit's URL as an unfetched
    // `resource` under its `site`, and the receipt annotates every minted id.
    #[tokio::test]
    async fn d3_persist_false_mints_nothing_persist_true_mints_unfetched_resources() {
        let (runtime, token, _dir) = test_runtime().await;
        let mut cfg = WebSectionConfig::default();
        cfg.search_providers.push(WebSearchProviderConfig::Fixture {
            name: "canned".to_string(),
            default: true,
            results: vec![WebFixtureResult {
                title: "Result".into(),
                url: "https://found.example.test/page".into(),
                snippet: "s".into(),
            }],
        });

        let not_persisted = run_search(&runtime, &token, &cfg, params("q", None, None, false))
            .await
            .expect("search dispatches");
        let not_persisted_id =
            uuid::Uuid::parse_str(not_persisted["receipt_id"].as_str().unwrap()).unwrap();
        let not_persisted_annotates = runtime
            .neighbors(
                &token,
                not_persisted_id,
                khive_storage::Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            not_persisted_annotates.len(),
            0,
            "persist=false annotates nothing"
        );

        let persisted = run_search(&runtime, &token, &cfg, params("q", None, None, true))
            .await
            .expect("search dispatches");
        let persisted_id =
            uuid::Uuid::parse_str(persisted["receipt_id"].as_str().unwrap()).unwrap();
        let persisted_annotates = runtime
            .neighbors(
                &token,
                persisted_id,
                khive_storage::Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(persisted_annotates.len(), 1);
        let minted_id = persisted_annotates[0].node_id;
        let entity = runtime
            .entities(&token)
            .unwrap()
            .get_entity(minted_id)
            .await
            .unwrap()
            .expect("the hit was minted as an entity");
        assert_eq!(entity.entity_type.as_deref(), Some("resource"));
        assert_eq!(entity.properties.unwrap()["status"], Value::Null);

        let site_neighbors = runtime
            .neighbors(
                &token,
                minted_id,
                khive_storage::Direction::In,
                None,
                Some(vec![EdgeRelation::Contains]),
            )
            .await
            .unwrap();
        assert_eq!(
            site_neighbors.len(),
            1,
            "the resource is contained by exactly one site"
        );
    }

    // arm 29 (search half): a receipt failure (secret gate refusal, the
    // provider's own result title carries an AKIA-shaped token) leaves no
    // note behind; an ordinary search receipt is the positive control.
    #[tokio::test]
    async fn arm29_search_receipt_failure_leaves_no_note_success_control() {
        let (runtime, token, _dir) = test_runtime().await;
        let before = runtime
            .list_notes(&token, Some("observation"), 100, 0)
            .await
            .unwrap()
            .len();

        let mut cfg = WebSectionConfig::default();
        cfg.search_providers.push(WebSearchProviderConfig::Fixture {
            name: "secret".to_string(),
            default: true,
            results: vec![WebFixtureResult {
                // gitleaks:allow - fixture value, matches khive-runtime's own
                // secret_gate test literal.
                title: "AKIAFAKEKEY1234567890".to_string(),
                url: "https://x.test/".into(),
                snippet: "s".into(),
            }],
        });
        let err = run_search(&runtime, &token, &cfg, params("q", None, None, false))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("receipt write failed"), "{err}");
        let after = runtime
            .list_notes(&token, Some("observation"), 100, 0)
            .await
            .unwrap()
            .len();
        assert_eq!(after, before, "the refused receipt left no note behind");

        let mut clean_cfg = WebSectionConfig::default();
        clean_cfg
            .search_providers
            .push(WebSearchProviderConfig::Fixture {
                name: "clean".to_string(),
                default: true,
                results: vec![WebFixtureResult {
                    title: "fine".into(),
                    url: "https://x.test/".into(),
                    snippet: "s".into(),
                }],
            });
        let ok = run_search(&runtime, &token, &clean_cfg, params("q", None, None, false))
            .await
            .expect("clean receipt succeeds");
        assert!(ok["receipt_id"].is_string());
        let after2 = runtime
            .list_notes(&token, Some("observation"), 100, 0)
            .await
            .unwrap()
            .len();
        assert_eq!(after2, before + 1);
    }
}
