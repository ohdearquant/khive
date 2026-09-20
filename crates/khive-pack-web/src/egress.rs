//! Address, scheme, allowlist, credential and header policy for `web.fetch`
//! and `web.search` (ADR-175 Amendment 1, A1.2). Pure decision logic: no
//! outbound networking happens in this module. The only I/O is DNS
//! resolution, reached through the injectable [`Resolver`] trait so policy
//! is fully testable without touching the network (D6.6).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use async_trait::async_trait;
use khive_runtime::engine_config::{WebCredentialConfig, WebSectionConfig};
use url::Url;

/// A policy refusal: a stable machine-readable `code` plus a human message.
/// Every refusal in this pack is surfaced to the caller as
/// `RuntimeError::InvalidInput("{code}: {message}")`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl From<Refusal> for khive_runtime::RuntimeError {
    fn from(refusal: Refusal) -> Self {
        khive_runtime::RuntimeError::InvalidInput(refusal.to_string())
    }
}

/// DNS resolution, injected so tests never touch the real network (D6.6).
///
/// Called twice per hop by [`resolve_and_pin`]: once to validate the answer
/// against the address-class rules (A1.2.2), once immediately before the
/// connection is pinned, refusing on any disagreement between the two
/// (defends against a resolver that answers differently on the second call —
/// A1.2.2's "resolution changes between the check and the connect").
#[async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String>;
}

/// Production resolver: ordinary system DNS via `tokio::net::lookup_host`.
pub struct SystemResolver;

#[async_trait]
impl Resolver for SystemResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        tokio::net::lookup_host((host, 0))
            .await
            .map(|iter| iter.map(|addr| addr.ip()).collect())
            .map_err(|error| error.to_string())
    }
}

/// Classify a resolved address against A1.2.2's disallowed classes. Returns
/// `Some((code, human class name))` when the address must refuse, `None`
/// when it is an ordinary public address.
///
/// `100.64.0.0/10` (the shared/CGNAT address space) counts as private per the
/// ADR even though `Ipv4Addr::is_private` does not cover it.
pub fn classify_address(addr: IpAddr) -> Option<(&'static str, &'static str)> {
    match addr {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                return Some(("address_loopback", "loopback"));
            }
            if v4.is_link_local() {
                return Some(("address_link_local", "link-local"));
            }
            if v4.is_private() {
                return Some(("address_private", "private"));
            }
            if is_shared_address_space(v4) {
                return Some(("address_private", "shared address space (100.64.0.0/10)"));
            }
            if v4.is_multicast() {
                return Some(("address_multicast", "multicast"));
            }
            if v4.is_broadcast() {
                return Some(("address_broadcast", "broadcast"));
            }
            if v4.is_unspecified() {
                return Some(("address_unspecified", "unspecified"));
            }
            None
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return Some(("address_loopback", "loopback"));
            }
            if v6.is_unspecified() {
                return Some(("address_unspecified", "unspecified"));
            }
            if v6.is_multicast() {
                return Some(("address_multicast", "multicast"));
            }
            let segments = v6.segments();
            // fe80::/10 — link-local unicast.
            if segments[0] & 0xffc0 == 0xfe80 {
                return Some(("address_link_local", "link-local"));
            }
            // fc00::/7 — unique local (RFC 4193).
            if segments[0] & 0xfe00 == 0xfc00 {
                return Some(("address_unique_local", "unique-local"));
            }
            // ::ffff:0:0/96 — IPv4-mapped; classify the embedded IPv4 address.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return classify_address(IpAddr::V4(v4));
            }
            None
        }
    }
}

fn is_shared_address_space(v4: Ipv4Addr) -> bool {
    let octets = v4.octets();
    octets[0] == 100 && (octets[1] & 0b1100_0000) == 64
}

/// A1.2.1: scheme is `http`/`https` only, and no URL in the chain (including
/// redirect targets) may carry userinfo.
pub fn check_scheme_and_userinfo(url: &Url) -> Result<(), Refusal> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(Refusal::new(
            "scheme_not_allowed",
            format!(
                "scheme {:?} is not allowed; only http and https are permitted",
                url.scheme()
            ),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Refusal::new(
            "userinfo_present",
            "URLs carrying userinfo (user:password@host) are refused",
        ));
    }
    Ok(())
}

/// Normalize a host for comparison: lowercase, trailing dot stripped.
pub fn normalize_host(host: &str) -> String {
    let lower = host.to_ascii_lowercase();
    lower.trim_end_matches('.').to_string()
}

/// A1.2.3: with no allowlist configured, every host is reachable subject to
/// the other rules. With one configured, it is exclusive.
pub fn check_allowlist(host: &str, cfg: &WebSectionConfig) -> Result<(), Refusal> {
    if cfg.allowlist.is_empty() {
        return Ok(());
    }
    let host = normalize_host(host);
    let allowed = cfg
        .allowlist
        .iter()
        .any(|entry| normalize_host(&entry.host) == host);
    if allowed {
        Ok(())
    } else {
        Err(Refusal::new(
            "host_not_allowlisted",
            format!("host {host:?} is not in the configured allowlist"),
        ))
    }
}

/// A1.2.2 + defense against DNS-rebinding-shaped TOCTOU: resolve `host`,
/// refuse if any returned address is disallowed, then resolve again and
/// refuse if the chosen address is absent from the second answer. Returns
/// the address to pin the connection to.
pub async fn resolve_and_pin(resolver: &dyn Resolver, host: &str) -> Result<IpAddr, Refusal> {
    let first = resolver
        .resolve(host)
        .await
        .map_err(|error| Refusal::new("resolution_failed", format!("{host}: {error}")))?;
    if first.is_empty() {
        return Err(Refusal::new(
            "resolution_failed",
            format!("{host} resolved to no addresses"),
        ));
    }
    for addr in &first {
        if let Some((code, class)) = classify_address(*addr) {
            return Err(Refusal::new(
                code,
                format!("{host} resolved to {addr}, a {class} address"),
            ));
        }
    }
    let chosen = first[0];
    let second = resolver
        .resolve(host)
        .await
        .map_err(|error| Refusal::new("resolution_failed", format!("{host}: {error}")))?;
    if !second.contains(&chosen) {
        return Err(Refusal::new(
            "resolution_unstable",
            format!("{host} resolution changed between the check and the connect"),
        ));
    }
    Ok(chosen)
}

/// A1.2.6: does `host` fall inside `credential.hosts`? An IP-literal entry
/// matches only that exact address; a hostname entry matches itself or any
/// name ending in `.{entry}` at a label boundary.
pub fn credential_host_allowed(credential: &WebCredentialConfig, host: &str) -> bool {
    let host = normalize_host(host);
    // An IP-literal host matches only an exact entry, never a suffix — even
    // when the configured entry happens not to parse as an IP itself (a
    // fragment like "0.113.9" must not act as a suffix over "203.0.113.9").
    // A1.2.6: IP literals are exact-address entries only, never suffix
    // matches.
    let host_is_ip = host.parse::<IpAddr>().is_ok();
    credential.hosts.iter().any(|entry| {
        let entry = normalize_host(entry);
        if host_is_ip {
            return host == entry;
        }
        host == entry || host.ends_with(&format!(".{entry}"))
    })
}

/// Resolve the named credential and validate it against `host`, per A1.2.6:
/// not-configured refuses naming the credential; configured-but-out-of-set
/// refuses naming both the credential and the host.
pub fn check_credential<'a>(
    cfg: &'a WebSectionConfig,
    name: &str,
    host: &str,
) -> Result<&'a WebCredentialConfig, Refusal> {
    let credential = cfg
        .credentials
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| {
            Refusal::new(
                "credential_not_configured",
                format!("credential {name:?} is not configured"),
            )
        })?;
    if credential_host_allowed(credential, host) {
        Ok(credential)
    } else {
        Err(Refusal::new(
            "credential_host_mismatch",
            format!("credential {name:?} is not scoped to host {host:?}"),
        ))
    }
}

/// A1.2.6's `https`-at-every-hop rule for a credential-bearing read.
pub fn check_credential_scheme(url: &Url) -> Result<(), Refusal> {
    if url.scheme() != "https" {
        return Err(Refusal::new(
            "credential_requires_https",
            "a credential-bearing request requires https at every hop",
        ));
    }
    Ok(())
}

/// Headers a caller may set on an outbound `web.fetch` request (A1.2.7),
/// lowercase for case-insensitive comparison.
pub const ALLOWED_REQUEST_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "if-none-match",
    "if-modified-since",
    "user-agent",
];

/// Headers that carry credentials by name and are refused with a reason
/// naming `credential` as the only sanctioned path for a secret.
const CREDENTIAL_SHAPED_HEADERS: &[&str] = &["authorization", "cookie", "proxy-authorization"];

/// A1.2.7: validate and normalize the caller's `headers` argument, returning
/// the allow-listed `(name, value)` pairs to actually send.
pub fn check_headers(headers: &BTreeMap<String, String>) -> Result<Vec<(String, String)>, Refusal> {
    let mut out = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if CREDENTIAL_SHAPED_HEADERS.contains(&lower.as_str()) {
            return Err(Refusal::new(
                "header_not_allowed",
                format!(
                    "header {name:?} may not be set directly; use the credential argument instead"
                ),
            ));
        }
        if !ALLOWED_REQUEST_HEADERS.contains(&lower.as_str()) {
            return Err(Refusal::new(
                "header_not_allowed",
                format!("header {name:?} is not in the allowed request header set"),
            ));
        }
        out.push((name.clone(), value.clone()));
    }
    Ok(out)
}

/// Resolved (defaulted) operator ceilings for one `web.fetch`/`web.search` call.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedCeilings {
    pub timeout_default_s: u64,
    pub timeout_max_s: u64,
    pub max_bytes_default: u64,
    pub max_bytes_max: u64,
    pub search_limit_default: u32,
    pub search_limit_max: u32,
}

/// Built-in defaults used when the operator leaves a `[web]` ceiling unset.
pub const DEFAULT_TIMEOUT_S: u64 = 30;
pub const DEFAULT_TIMEOUT_MAX_S: u64 = 120;
pub const DEFAULT_MAX_BYTES: u64 = 5 * 1024 * 1024;
pub const DEFAULT_MAX_BYTES_MAX: u64 = 50 * 1024 * 1024;
pub const DEFAULT_SEARCH_LIMIT: u32 = 10;
pub const DEFAULT_SEARCH_LIMIT_MAX: u32 = 50;

pub fn resolve_ceilings(cfg: &WebSectionConfig) -> ResolvedCeilings {
    ResolvedCeilings {
        timeout_default_s: cfg.timeout_default_s.unwrap_or(DEFAULT_TIMEOUT_S),
        timeout_max_s: cfg.timeout_max_s.unwrap_or(DEFAULT_TIMEOUT_MAX_S),
        max_bytes_default: cfg.max_bytes_default.unwrap_or(DEFAULT_MAX_BYTES),
        max_bytes_max: cfg.max_bytes_max.unwrap_or(DEFAULT_MAX_BYTES_MAX),
        search_limit_default: cfg.search_limit_default.unwrap_or(DEFAULT_SEARCH_LIMIT),
        search_limit_max: cfg.search_limit_max.unwrap_or(DEFAULT_SEARCH_LIMIT_MAX),
    }
}

/// A1.2.5 / A1.3: a caller-supplied bound may lower the effective value but
/// never raise it above the operator ceiling; omitted uses the default.
pub fn check_ceiling(
    caller: Option<u64>,
    default: u64,
    ceiling: u64,
    param: &'static str,
) -> Result<u64, Refusal> {
    match caller {
        None => Ok(default),
        Some(value) => {
            if value > ceiling {
                Err(Refusal::new(
                    "ceiling_exceeded",
                    format!("{param}={value} exceeds the configured ceiling of {ceiling}"),
                ))
            } else {
                Ok(value)
            }
        }
    }
}

pub fn check_limit_ceiling(
    caller: Option<u32>,
    default: u32,
    ceiling: u32,
) -> Result<u32, Refusal> {
    match caller {
        None => Ok(default),
        Some(value) => {
            if value > ceiling {
                Err(Refusal::new(
                    "ceiling_exceeded",
                    format!("limit={value} exceeds the configured ceiling of {ceiling}"),
                ))
            } else {
                Ok(value)
            }
        }
    }
}

/// Pin a socket address for `host` on a per-request reqwest client, so the
/// physical connection lands on exactly the address [`resolve_and_pin`]
/// validated — never a fresh resolution of the name (A1.2.2).
pub fn pinned_client(
    host: &str,
    addr: IpAddr,
    port: u16,
    timeout: std::time::Duration,
) -> Result<reqwest::Client, Refusal> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        // A1.2.5: the byte bound is on decompressed bytes — a small
        // compressed response can expand without limit, so transparent
        // decompression has to happen before the truncation loop ever sees
        // a byte count.
        .gzip(true)
        .resolve(host, SocketAddr::new(addr, port))
        .build()
        .map_err(|error| Refusal::new("internal_client_build_failed", error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn cfg_with_allowlist(hosts: &[&str]) -> WebSectionConfig {
        WebSectionConfig {
            allowlist: hosts
                .iter()
                .map(|h| khive_runtime::engine_config::WebAllowlistEntry {
                    host: h.to_string(),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn credential(name: &str, hosts: &[&str]) -> WebCredentialConfig {
        WebCredentialConfig {
            name: name.to_string(),
            env_var: "UNUSED".to_string(),
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
        }
    }

    // arm 9: loopback refuses naming the resolved address; a public address
    // is the positive control in the same test.
    #[test]
    fn classify_address_arm9_loopback_refuses_public_allows() {
        assert!(classify_address(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))).is_some());
        assert!(classify_address(IpAddr::V6(Ipv6Addr::LOCALHOST)).is_some());
        assert!(classify_address(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))).is_none());
    }

    #[test]
    fn classify_address_covers_every_disallowed_class() {
        let cases: &[(IpAddr, &str)] = &[
            (IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), "address_loopback"),
            (
                IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1)),
                "address_link_local",
            ),
            (IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), "address_private"),
            (IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)), "address_private"),
            (IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), "address_private"),
            (IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)), "address_private"),
            (
                IpAddr::V4(Ipv4Addr::new(100, 127, 255, 254)),
                "address_private",
            ),
            (IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1)), "address_multicast"),
            (
                IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)),
                "address_broadcast",
            ),
            (IpAddr::V4(Ipv4Addr::UNSPECIFIED), "address_unspecified"),
            (IpAddr::V6(Ipv6Addr::LOCALHOST), "address_loopback"),
            (IpAddr::V6(Ipv6Addr::UNSPECIFIED), "address_unspecified"),
            (
                IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
                "address_link_local",
            ),
            (
                IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1)),
                "address_unique_local",
            ),
            (
                IpAddr::V6(Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1)),
                "address_multicast",
            ),
        ];
        for (addr, expected_code) in cases {
            let (code, _) = classify_address(*addr)
                .unwrap_or_else(|| panic!("expected {addr} to be disallowed"));
            assert_eq!(code, *expected_code, "address {addr}");
        }
        // Not in the shared range: 100.63.255.255 is one below 100.64.0.0/10.
        assert!(classify_address(IpAddr::V4(Ipv4Addr::new(100, 63, 255, 255))).is_none());
        // 100.128.0.0 is one above the /10 block.
        assert!(classify_address(IpAddr::V4(Ipv4Addr::new(100, 128, 0, 0))).is_none());
    }

    // arm 8: file:// refuses.
    #[test]
    fn arm8_file_scheme_refuses() {
        let url = Url::parse("file:///etc/passwd").unwrap();
        let err = check_scheme_and_userinfo(&url).unwrap_err();
        assert_eq!(err.code, "scheme_not_allowed");
    }

    #[test]
    fn only_http_and_https_pass() {
        for scheme in ["ftp://host/path", "data:text/plain,hi"] {
            let url = Url::parse(scheme).unwrap();
            assert!(check_scheme_and_userinfo(&url).is_err());
        }
        for scheme in ["http://host/path", "https://host/path"] {
            let url = Url::parse(scheme).unwrap();
            assert!(check_scheme_and_userinfo(&url).is_ok());
        }
    }

    // arm 24: userinfo refuses; the same URL without userinfo succeeds.
    #[test]
    fn arm24_userinfo_refuses_stripped_url_succeeds() {
        let with = Url::parse("https://user:pass@example.test/").unwrap();
        let err = check_scheme_and_userinfo(&with).unwrap_err();
        assert_eq!(err.code, "userinfo_present");

        let without = Url::parse("https://example.test/").unwrap();
        assert!(check_scheme_and_userinfo(&without).is_ok());
    }

    // arm 13: allowlist refuses outside, allows inside, in one test.
    #[test]
    fn arm13_allowlist_refuses_outside_allows_inside() {
        let cfg = cfg_with_allowlist(&["inside.test"]);
        assert!(check_allowlist("outside.test", &cfg).is_err());
        assert!(check_allowlist("inside.test", &cfg).is_ok());
    }

    #[test]
    fn no_allowlist_configured_permits_any_host() {
        let cfg = WebSectionConfig::default();
        assert!(check_allowlist("anything.test", &cfg).is_ok());
    }

    // arm 20: credential for a host outside its set refuses naming both;
    // the same credential to a host inside the set succeeds as the control.
    #[test]
    fn arm20_credential_host_mismatch_names_both() {
        let mut cfg = WebSectionConfig::default();
        cfg.credentials.push(credential("token", &["allowed.test"]));

        let err = check_credential(&cfg, "token", "other.test").unwrap_err();
        assert_eq!(err.code, "credential_host_mismatch");
        assert!(err.message.contains("token"));
        assert!(err.message.contains("other.test"));

        assert!(check_credential(&cfg, "token", "allowed.test").is_ok());
    }

    // arm 14: an unconfigured credential name refuses.
    #[test]
    fn arm14_unconfigured_credential_refuses() {
        let cfg = WebSectionConfig::default();
        let err = check_credential(&cfg, "missing", "anywhere.test").unwrap_err();
        assert_eq!(err.code, "credential_not_configured");
    }

    // arm 23: suffix matching at a label boundary, case/trailing-dot/port
    // insensitivity, and exact-only IP literals.
    #[test]
    fn arm23_suffix_matching_label_boundary_and_ip_literal_exactness() {
        let cred = credential("token", &["example.com"]);
        assert!(credential_host_allowed(&cred, "example.com"));
        assert!(credential_host_allowed(&cred, "api.example.com"));
        assert!(!credential_host_allowed(&cred, "evilexample.com"));
        // Mixed case and a trailing dot preserve the decision.
        assert!(credential_host_allowed(&cred, "API.Example.Com."));
        assert!(!credential_host_allowed(&cred, "EVILexample.com"));

        let ip_cred = credential("ip-token", &["203.0.113.9"]);
        assert!(credential_host_allowed(&ip_cred, "203.0.113.9"));
        assert!(!credential_host_allowed(&ip_cred, "203.0.113.10"));
        // An IP-literal entry never acts as a suffix.
        let suffixy = credential("suffix-ip", &["0.113.9"]);
        assert!(!credential_host_allowed(&suffixy, "203.0.113.9"));
    }

    // arm 22: https required at every hop when a credential is in play.
    #[test]
    fn arm22_credential_requires_https() {
        let http = Url::parse("http://example.test/").unwrap();
        assert_eq!(
            check_credential_scheme(&http).unwrap_err().code,
            "credential_requires_https"
        );
        let https = Url::parse("https://example.test/").unwrap();
        assert!(check_credential_scheme(&https).is_ok());
    }

    // arm 17: Authorization refuses naming credential; an allow-listed
    // header succeeds in the same test.
    #[test]
    fn arm17_authorization_header_refuses_allowed_header_succeeds() {
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), "Bearer x".to_string());
        let err = check_headers(&headers).unwrap_err();
        assert_eq!(err.code, "header_not_allowed");
        assert!(err.message.contains("credential"));

        let mut ok_headers = BTreeMap::new();
        ok_headers.insert("Accept".to_string(), "application/json".to_string());
        let allowed = check_headers(&ok_headers).unwrap();
        assert_eq!(
            allowed,
            vec![("Accept".to_string(), "application/json".to_string())]
        );
    }

    #[test]
    fn cookie_and_proxy_authorization_also_refuse_by_name() {
        for header in ["Cookie", "Proxy-Authorization"] {
            let mut headers = BTreeMap::new();
            headers.insert(header.to_string(), "x".to_string());
            let err = check_headers(&headers).unwrap_err();
            assert_eq!(err.code, "header_not_allowed");
        }
    }

    #[test]
    fn unlisted_header_refuses_by_name() {
        let mut headers = BTreeMap::new();
        headers.insert("X-Custom".to_string(), "x".to_string());
        let err = check_headers(&headers).unwrap_err();
        assert_eq!(err.code, "header_not_allowed");
        assert!(err.message.contains("X-Custom"));
    }

    // arm 25: max_bytes/timeout_s/limit ceilings — over refuses naming the
    // parameter and ceiling; equal to and below succeed; omitted uses the
    // operator default.
    #[test]
    fn arm25_ceilings_refuse_above_succeed_at_and_below_default_when_omitted() {
        let err = check_ceiling(Some(101), 30, 100, "max_bytes").unwrap_err();
        assert_eq!(err.code, "ceiling_exceeded");
        assert!(err.message.contains("max_bytes"));
        assert!(err.message.contains("100"));
        assert_eq!(check_ceiling(Some(100), 30, 100, "max_bytes").unwrap(), 100);
        assert_eq!(check_ceiling(Some(1), 30, 100, "max_bytes").unwrap(), 1);
        assert_eq!(check_ceiling(None, 30, 100, "max_bytes").unwrap(), 30);

        assert_eq!(
            check_limit_ceiling(Some(51), 10, 50).unwrap_err().code,
            "ceiling_exceeded"
        );
        assert_eq!(check_limit_ceiling(Some(50), 10, 50).unwrap(), 50);
        assert_eq!(check_limit_ceiling(None, 10, 50).unwrap(), 10);
    }

    struct MapResolver {
        answers: std::sync::Mutex<Vec<Vec<IpAddr>>>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Resolver for MapResolver {
        async fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut answers = self.answers.lock().unwrap();
            if answers.len() > 1 {
                Ok(answers.remove(0))
            } else {
                Ok(answers[0].clone())
            }
        }
    }

    fn public_addr(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(93, 184, 216, last))
    }

    // arm 18: resolution changing between the check and the connect
    // refuses; a stable-resolution host is the positive control.
    #[tokio::test]
    async fn arm18_unstable_resolution_refuses_stable_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let unstable = MapResolver {
            answers: std::sync::Mutex::new(vec![vec![public_addr(1)], vec![public_addr(2)]]),
            calls: calls.clone(),
        };
        let err = resolve_and_pin(&unstable, "unstable.test")
            .await
            .unwrap_err();
        assert_eq!(err.code, "resolution_unstable");
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let stable = MapResolver {
            answers: std::sync::Mutex::new(vec![vec![public_addr(1)]]),
            calls: calls.clone(),
        };
        let addr = resolve_and_pin(&stable, "stable.test").await.unwrap();
        assert_eq!(addr, public_addr(1));
    }

    // arm 9 (integration half): resolving to loopback refuses through the
    // full resolve_and_pin path, naming the resolved address.
    #[tokio::test]
    async fn arm9_resolve_and_pin_refuses_loopback_allows_public() {
        let calls = Arc::new(AtomicUsize::new(0));
        let loopback = MapResolver {
            answers: std::sync::Mutex::new(vec![vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]]),
            calls: calls.clone(),
        };
        let err = resolve_and_pin(&loopback, "loopback.test")
            .await
            .unwrap_err();
        assert_eq!(err.code, "address_loopback");
        assert!(err.message.contains("127.0.0.1"));

        let public = MapResolver {
            answers: std::sync::Mutex::new(vec![vec![public_addr(1)]]),
            calls,
        };
        assert!(resolve_and_pin(&public, "public.test").await.is_ok());
    }

    // arm 10: a redirect chain whose second hop points into private address
    // space refuses at that hop; a same-length public chain is the control.
    // The per-hop mechanism is resolve_and_pin re-run for each hop's host —
    // exercised here directly since egress.rs owns no redirect loop itself.
    #[tokio::test]
    async fn arm10_second_hop_private_refuses_public_chain_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let hop1 = MapResolver {
            answers: std::sync::Mutex::new(vec![vec![public_addr(1)]]),
            calls: calls.clone(),
        };
        assert!(resolve_and_pin(&hop1, "hop1.test").await.is_ok());
        let hop2_private = MapResolver {
            answers: std::sync::Mutex::new(vec![vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))]]),
            calls: calls.clone(),
        };
        let err = resolve_and_pin(&hop2_private, "hop2.test")
            .await
            .unwrap_err();
        assert_eq!(err.code, "address_private");

        let hop2_public = MapResolver {
            answers: std::sync::Mutex::new(vec![vec![public_addr(2)]]),
            calls,
        };
        assert!(resolve_and_pin(&hop2_public, "hop2-public.test")
            .await
            .is_ok());
    }

    #[test]
    fn normalize_host_lowercases_and_strips_trailing_dot() {
        assert_eq!(normalize_host("Example.COM."), "example.com");
    }
}
