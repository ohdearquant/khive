//! Deterministic identity for web entities (ADR-191 D1).
//!
//! `site` keys on `(scheme, host, port)`; `page` and `resource` key on
//! `(site, canonical path+query)` — the SAME formula for both subtypes,
//! deliberately: D3 re-types an unfetched `resource` to `page` IN PLACE when
//! `fetch` later learns the body is HTML, and identity must not change when
//! that happens ("the id does not change because identity is by address").
//! Subtype is a property of content, never an input to the id.

use uuid::Uuid;

/// Fixed namespace UUID for every `Uuid::new_v5` computed by this pack.
/// Arbitrary but fixed: nothing depends on its value beyond that repeated
/// calls with the same identity tuple converge on the same output.
pub const WEB_NAMESPACE: Uuid = Uuid::from_u128(0x1910_adb1_91ad_5eb0_91ad_b191_05eb_0adb);

/// Canonicalize a URL per D1: scheme and host lowercased, default port
/// dropped, path percent-normalized, query kept with keys sorted, fragment
/// dropped. The `url` crate already lowercases scheme/host and omits a
/// default port during parsing (WHATWG URL spec), and its path is already
/// percent-normalized on parse — only fragment-stripping and query-key
/// sorting are this function's own work.
pub fn canonicalize(mut url: url::Url) -> url::Url {
    url.set_fragment(None);
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if pairs.is_empty() {
        url.set_query(None);
        return url;
    }
    let mut sorted = pairs;
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in &sorted {
        serializer.append_pair(k, v);
    }
    url.set_query(Some(&serializer.finish()));
    url
}

/// `scheme://host:port` for a canonicalized URL — the `site` identity key.
/// `url::Url::port_or_known_default` folds a default port (80/443) away
/// already; an explicit non-default port is preserved.
pub fn site_key(url: &url::Url) -> String {
    format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.port_or_known_default().unwrap_or(0),
    )
}

/// Deterministic id for the `site` entity owning `url`.
pub fn site_id(url: &url::Url) -> Uuid {
    Uuid::new_v5(&WEB_NAMESPACE, format!("site|{}", site_key(url)).as_bytes())
}

/// Canonical path+query string used as the second half of the `page`/
/// `resource` identity tuple. Never includes the fragment (dropped by
/// [`canonicalize`] before this is read).
pub fn path_and_query(url: &url::Url) -> String {
    match url.query() {
        Some(q) if !q.is_empty() => format!("{}?{}", url.path(), q),
        _ => url.path().to_string(),
    }
}

/// Deterministic id for the `page`/`resource` document identified by
/// `(site, path_and_query)`. Shared by both subtypes on purpose (see module
/// doc) — the caller decides `entity_type` from content, not from this id.
pub fn document_id(site: Uuid, path_and_query: &str) -> Uuid {
    Uuid::new_v5(
        &WEB_NAMESPACE,
        format!("document|{site}|{path_and_query}").as_bytes(),
    )
}

/// Deterministic id for the `resource` produced by `web.extract`'s `text`
/// kind over `original` (ADR-191 D3: "text `resource` (`derived_from`)").
/// The derived text has no URL of its own, so it keys on the originating
/// document's id rather than an address — repeated `extract(text)` calls
/// over the same document converge on one row instead of minting a new one
/// each time, matching every other identity in this module.
pub fn derived_text_id(original: Uuid) -> Uuid {
    Uuid::new_v5(
        &WEB_NAMESPACE,
        format!("derived-text|{original}").as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn canonicalize_drops_fragment_default_port_and_sorts_query() {
        let url = Url::parse("HTTPS://Example.COM:443/a/b?z=1&a=2#top").unwrap();
        let out = canonicalize(url);
        assert_eq!(out.scheme(), "https");
        assert_eq!(out.host_str(), Some("example.com"));
        assert_eq!(out.port(), None, "default port must not be stored");
        assert_eq!(out.fragment(), None);
        assert_eq!(out.query(), Some("a=2&z=1"), "query keys sorted");
    }

    #[test]
    fn distinct_query_values_are_distinct_resources_fragment_alone_is_not() {
        let base = canonicalize(Url::parse("https://example.com/x?id=1").unwrap());
        let same_fragment_only =
            canonicalize(Url::parse("https://example.com/x?id=1#section").unwrap());
        let different_value = canonicalize(Url::parse("https://example.com/x?id=2").unwrap());

        let site = site_id(&base);
        let id_a = document_id(site, &path_and_query(&base));
        let id_b = document_id(site, &path_and_query(&same_fragment_only));
        let id_c = document_id(site, &path_and_query(&different_value));

        assert_eq!(id_a, id_b, "#section must not change identity");
        assert_ne!(id_a, id_c, "?id=1 and ?id=2 are distinct resources");
    }

    #[test]
    fn document_id_is_independent_of_subtype_by_construction() {
        // document_id takes no subtype argument at all — the same call
        // computes the id whether the caller intends `page` or `resource`,
        // which is what lets fetch re-type a row in place.
        let url = canonicalize(Url::parse("https://example.com/robots.txt").unwrap());
        let site = site_id(&url);
        let first = document_id(site, &path_and_query(&url));
        let second = document_id(site, &path_and_query(&url));
        assert_eq!(first, second);
    }

    #[test]
    fn explicit_non_default_port_is_preserved_in_site_identity() {
        let default_port = canonicalize(Url::parse("https://example.com/").unwrap());
        let explicit_port = canonicalize(Url::parse("https://example.com:8443/").unwrap());
        assert_ne!(site_id(&default_port), site_id(&explicit_port));
    }

    #[test]
    fn same_tuple_from_independent_parses_converges() {
        let a = canonicalize(Url::parse("https://example.com/a?b=1&a=2").unwrap());
        let b = canonicalize(Url::parse("https://EXAMPLE.com:443/a?a=2&b=1").unwrap());
        assert_eq!(site_id(&a), site_id(&b));
        assert_eq!(
            document_id(site_id(&a), &path_and_query(&a)),
            document_id(site_id(&b), &path_and_query(&b))
        );
    }
}
