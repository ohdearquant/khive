//! `web.extract` (ADR-191 D3, D2).
//!
//! Parses an already-fetched body (never fetches one itself — that is
//! `web.fetch`'s job) into whichever of `text`/`links`/`sitemap`/`feed` the
//! caller names, default all applicable to the stored content-type. No HTML
//! or XML parser crate is a workspace dependency, so every extraction here
//! is a bounded regex over the raw bytes rather than a DOM walk.
//!
//! - `links`: every `<a href="...">` in an HTML body becomes a
//!   `page links_to page|resource` edge (D2's new base row) to a target
//!   minted, if absent, as an unfetched `resource` (`status: null`) — never
//!   overwritten if the target already exists and has been fetched.
//! - `sitemap`/`feed`: every `<loc>`/`<link>` entry becomes a `resource`
//!   under the document's own `site`, linked `site contains resource` (the
//!   pack's second `EDGE_RULES` row) — a feed/sitemap entry is the site's
//!   content, not the feed document's.
//! - `text`: a new `resource` holding the tag-stripped text, linked
//!   `document derived_from document` (source: the new text resource,
//!   target: the original) and keyed by [`identity::derived_text_id`] so
//!   repeated extraction over an unchanged document converges on one row.

use std::sync::LazyLock;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::EdgeRelation;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;

use crate::egress::Refusal;
use crate::identity;
use crate::WebPack;

const MAX_TEXT_EXCERPT_BYTES: usize = 200_000;

static HREF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<a\s[^>]*?href\s*=\s*["']([^"'#][^"']*)["']"#).expect("valid regex")
});
static LOC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<loc>\s*([^<\s][^<]*?)\s*</loc>"#).expect("valid regex"));
static ATOM_LINK_HREF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<link\b[^>]*\bhref\s*=\s*["']([^"']+)["'][^>]*/?>"#).expect("valid regex")
});
static RSS_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<link>\s*([^<\s][^<]*?)\s*</link>"#).expect("valid regex"));
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]+>").expect("valid regex"));
/// `<script>`/`<style>` bodies are never prose: stripped whole (tag and
/// content) before `TAG_RE`'s generic tag-only strip runs, so their
/// contents never leak into extracted text. Two alternatives, not a
/// backreference — the `regex` crate's engine is backtracking-free and
/// does not support `\1`.
static SCRIPT_STYLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<script\b[^>]*>.*?</script>|<style\b[^>]*>.*?</style>").expect("valid regex")
});
static WHITESPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("valid regex"));

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtractParams {
    #[serde(default)]
    id: Option<Uuid>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    kinds: Option<Vec<String>>,
    #[serde(default)]
    namespace: Option<String>,
}

const ALL_KINDS: &[&str] = &["text", "links", "sitemap", "feed"];

fn applicable_kinds(entity_type: &str, content_type: Option<&str>) -> Vec<&'static str> {
    let content_type = content_type.unwrap_or_default().to_ascii_lowercase();
    let is_feed_or_sitemap = content_type.contains("xml") || content_type.contains("rss");
    match entity_type {
        "page" => vec!["links", "text"],
        _ if is_feed_or_sitemap => vec!["sitemap", "feed"],
        _ => vec!["text"],
    }
}

async fn resolve_target(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: &ExtractParams,
) -> Result<(Uuid, khive_storage::Entity), RuntimeError> {
    let id = match (params.id, &params.url) {
        (Some(id), None) => id,
        (None, Some(url_str)) => {
            let url = Url::parse(url_str)
                .map_err(|error| RuntimeError::InvalidInput(format!("invalid url: {error}")))?;
            let canonical = identity::canonicalize(url);
            let site = identity::site_id(&canonical);
            identity::document_id(site, &identity::path_and_query(&canonical))
        }
        (Some(_), Some(_)) => {
            return Err(RuntimeError::InvalidInput(
                "web.extract: pass exactly one of id or url, not both".to_string(),
            ))
        }
        (None, None) => {
            return Err(RuntimeError::InvalidInput(
                "web.extract: id or url is required".to_string(),
            ))
        }
    };
    let entity = runtime
        .entities(token)?
        .get_entity(id)
        .await?
        .ok_or_else(|| {
            RuntimeError::from(Refusal::new(
                "not_found",
                format!("web.extract: no document at id {id}"),
            ))
        })?;
    Ok((id, entity))
}

fn resolve_against(base: &Url, href: &str) -> Option<Url> {
    base.join(href).ok()
}

async fn extract_links(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    document_id: Uuid,
    base_url: &Url,
    body: &str,
) -> Result<u32, RuntimeError> {
    let mut seen = std::collections::HashSet::new();
    let mut count = 0u32;
    for capture in HREF_RE.captures_iter(body) {
        let href = capture[1].trim();
        if href.is_empty() || href.starts_with("javascript:") || href.starts_with("mailto:") {
            continue;
        }
        let Some(target_url) = resolve_against(base_url, href) else {
            continue;
        };
        let canonical = identity::canonicalize(target_url);
        if canonical.scheme() != "http" && canonical.scheme() != "https" {
            continue;
        }
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let site = identity::site_id(&canonical);
        crate::entities::get_or_create(
            runtime,
            token,
            site,
            "service",
            "site",
            &identity::site_key(&canonical),
            json!({
                "scheme": canonical.scheme(),
                "host": canonical.host_str(),
                "port": canonical.port_or_known_default(),
            }),
        )
        .await?;
        let target_id = identity::document_id(site, &identity::path_and_query(&canonical));
        crate::entities::get_or_create(
            runtime,
            token,
            target_id,
            "document",
            "resource",
            canonical.as_ref(),
            json!({ "url": canonical.to_string(), "status": Value::Null }),
        )
        .await?;
        runtime
            .link(token, site, target_id, EdgeRelation::Contains, 1.0, None)
            .await?;
        runtime
            .link(
                token,
                document_id,
                target_id,
                EdgeRelation::LinksTo,
                1.0,
                None,
            )
            .await?;
        count += 1;
    }
    Ok(count)
}

async fn extract_entries(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    site_id: Uuid,
    body: &str,
    kind: &str,
) -> Result<u32, RuntimeError> {
    let mut urls: Vec<String> = Vec::new();
    match kind {
        "sitemap" => {
            urls.extend(LOC_RE.captures_iter(body).map(|c| c[1].trim().to_string()));
        }
        "feed" => {
            urls.extend(
                ATOM_LINK_HREF_RE
                    .captures_iter(body)
                    .map(|c| c[1].trim().to_string()),
            );
            urls.extend(
                RSS_LINK_RE
                    .captures_iter(body)
                    .map(|c| c[1].trim().to_string()),
            );
        }
        _ => {}
    }
    let mut seen = std::collections::HashSet::new();
    let mut count = 0u32;
    for raw in urls {
        let Ok(url) = Url::parse(&raw) else { continue };
        let canonical = identity::canonicalize(url);
        if canonical.scheme() != "http" && canonical.scheme() != "https" {
            continue;
        }
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let entry_site = identity::site_id(&canonical);
        let target_id = identity::document_id(entry_site, &identity::path_and_query(&canonical));
        crate::entities::get_or_create(
            runtime,
            token,
            target_id,
            "document",
            "resource",
            canonical.as_ref(),
            json!({ "url": canonical.to_string(), "status": Value::Null }),
        )
        .await?;
        // Entries belong to the SITE that published the feed/sitemap, which
        // is the source document's own site (D2: "site contains resource ...
        // extract (sitemap and feed entries)") — not necessarily the
        // entry's own site when the entry points elsewhere, so both edges
        // are recorded: containment under the publishing site, plus the
        // entry's own site if it differs.
        runtime
            .link(token, site_id, target_id, EdgeRelation::Contains, 1.0, None)
            .await?;
        if entry_site != site_id {
            crate::entities::get_or_create(
                runtime,
                token,
                entry_site,
                "service",
                "site",
                &identity::site_key(&canonical),
                json!({
                    "scheme": canonical.scheme(),
                    "host": canonical.host_str(),
                    "port": canonical.port_or_known_default(),
                }),
            )
            .await?;
            runtime
                .link(
                    token,
                    entry_site,
                    target_id,
                    EdgeRelation::Contains,
                    1.0,
                    None,
                )
                .await?;
        }
        count += 1;
    }
    Ok(count)
}

async fn extract_text(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    original_id: Uuid,
    original_url: &str,
    body: &str,
) -> Result<Uuid, RuntimeError> {
    let no_script_style = SCRIPT_STYLE_RE.replace_all(body, " ");
    let stripped = TAG_RE.replace_all(&no_script_style, " ");
    let collapsed = WHITESPACE_RE.replace_all(stripped.trim(), " ").to_string();
    // `MAX_TEXT_EXCERPT_BYTES` bounds BYTES, not chars — `.chars().take(N)`
    // would cap at N chars and let a multi-byte-heavy document (CJK text is
    // 3 bytes/char) through at up to 3-4x the intended byte budget. Truncate
    // by byte length instead, walking back to the nearest char boundary so a
    // multi-byte character is never split.
    let excerpt: String = if collapsed.len() <= MAX_TEXT_EXCERPT_BYTES {
        collapsed
    } else {
        let mut end = MAX_TEXT_EXCERPT_BYTES;
        while !collapsed.is_char_boundary(end) {
            end -= 1;
        }
        collapsed[..end].to_string()
    };

    let store = crate::blob_store(runtime)?;
    let content_ref = store
        .put(excerpt.clone().into_bytes())
        .await
        .map_err(RuntimeError::from)?;

    let text_id = identity::derived_text_id(original_id);
    crate::entities::get_or_create(
        runtime,
        token,
        text_id,
        "document",
        "resource",
        &format!("{original_url} (extracted text)"),
        json!({ "derived_from": original_id.to_string() }),
    )
    .await?;
    crate::entities::patch(
        runtime,
        token,
        text_id,
        Some("resource"),
        json!({
            "derived_from": original_id.to_string(),
            "content_type": "text/plain",
            "blob_ref": content_ref.to_string(),
            "size": excerpt.len() as u64,
        }),
    )
    .await?;
    runtime
        .link(
            token,
            text_id,
            original_id,
            EdgeRelation::DerivedFrom,
            1.0,
            None,
        )
        .await?;
    Ok(text_id)
}

async fn run_extract(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: ExtractParams,
) -> Result<Value, RuntimeError> {
    let (target_id, entity) = resolve_target(runtime, token, &params).await?;
    let properties = entity.properties.clone().unwrap_or(Value::Null);
    let content_ref = properties
        .get("blob_ref")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RuntimeError::from(Refusal::new(
                "not_fetched",
                format!("web.extract: {target_id} has no stored body; fetch it first"),
            ))
        })?;
    let content_ref = khive_storage::ContentRef::from_hex(content_ref)
        .map_err(|error| RuntimeError::Internal(format!("stored blob_ref is invalid: {error}")))?;
    let store = crate::blob_store(runtime)?;
    let bytes = store
        .get_bounded_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await
        .map_err(RuntimeError::from)?;
    let body = String::from_utf8_lossy(&bytes).into_owned();

    let url_str = properties
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let base_url = Url::parse(&url_str)
        .map_err(|error| RuntimeError::Internal(format!("stored url is invalid: {error}")))?;
    let site_id = identity::site_id(&identity::canonicalize(base_url.clone()));
    let entity_type = entity.entity_type.as_deref().unwrap_or("resource");
    let content_type = properties.get("content_type").and_then(Value::as_str);

    let kinds: Vec<String> = match params.kinds {
        Some(k) if !k.is_empty() => k,
        _ => applicable_kinds(entity_type, content_type)
            .into_iter()
            .map(str::to_string)
            .collect(),
    };
    for kind in &kinds {
        if !ALL_KINDS.contains(&kind.as_str()) {
            return Err(RuntimeError::InvalidInput(format!(
                "web.extract: unknown kind {kind:?}; expected one of {ALL_KINDS:?}"
            )));
        }
    }

    let mut result = serde_json::Map::new();
    for kind in &kinds {
        match kind.as_str() {
            "links" => {
                let count = extract_links(runtime, token, target_id, &base_url, &body).await?;
                result.insert("links".to_string(), json!({ "edges_created": count }));
            }
            "sitemap" => {
                let count = extract_entries(runtime, token, site_id, &body, "sitemap").await?;
                result.insert("sitemap".to_string(), json!({ "entries": count }));
            }
            "feed" => {
                let count = extract_entries(runtime, token, site_id, &body, "feed").await?;
                result.insert("feed".to_string(), json!({ "entries": count }));
            }
            "text" => {
                let text_id = extract_text(runtime, token, target_id, &url_str, &body).await?;
                result.insert("text".to_string(), json!({ "id": text_id.to_string() }));
            }
            _ => unreachable!("validated above"),
        }
    }

    Ok(json!({
        "id": target_id.to_string(),
        "kinds": kinds,
        "result": Value::Object(result),
    }))
}

impl WebPack {
    pub(crate) async fn handle_extract(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let params: ExtractParams = serde_json::from_value(params).map_err(|error| {
            RuntimeError::InvalidInput(format!("invalid web.extract arguments: {error}"))
        })?;
        let effective_token =
            crate::fetch::resolve_effective_token(token, params.namespace.as_deref())?;
        run_extract(&self.runtime, &effective_token, params).await
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
        builder.register(crate::WebPack::new(runtime.clone()));
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

    async fn seed_page(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        url_str: &str,
        content_type: &str,
        body: &[u8],
    ) -> Uuid {
        let url = Url::parse(url_str).unwrap();
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
        let entity_type = if content_type.starts_with("text/html") {
            "page"
        } else {
            "resource"
        };
        crate::entities::get_or_create(
            runtime,
            token,
            id,
            "document",
            entity_type,
            canonical.as_ref(),
            json!({ "url": canonical.to_string() }),
        )
        .await
        .unwrap();
        crate::entities::patch(
            runtime,
            token,
            id,
            Some(entity_type),
            json!({
                "url": canonical.to_string(),
                "content_type": content_type,
                "blob_ref": content_ref.to_string(),
            }),
        )
        .await
        .unwrap();
        id
    }

    // A2: extract(links) on a page with N distinct hrefs yields N links_to
    // edges whose targets are minted as unfetched resources; a repeated
    // href is not double-counted (dedup), and a fragment-only href is
    // skipped as not a distinct resource.
    #[tokio::test]
    async fn a2_extract_links_yields_n_edges_to_unfetched_resources_dedup_and_fragment_skip() {
        let (runtime, token, _dir) = test_runtime().await;
        let html = br##"<html><body>
            <a href="/a">A</a>
            <a href="/b">B</a>
            <a href="/a">A again</a>
            <a href="#top">fragment only</a>
            <a href="https://other.example.test/c">C</a>
        </body></html>"##;
        let page_id = seed_page(
            &runtime,
            &token,
            "https://origin.example.test/",
            "text/html",
            html,
        )
        .await;

        let reply = run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(page_id),
                url: None,
                kinds: Some(vec!["links".to_string()]),
                namespace: None,
            },
        )
        .await
        .expect("extract succeeds");
        assert_eq!(
            reply["result"]["links"]["edges_created"], 3,
            "a, b, c — deduped, fragment skipped"
        );

        let neighbors = runtime
            .neighbors(
                &token,
                page_id,
                khive_storage::Direction::Out,
                None,
                Some(vec![EdgeRelation::LinksTo]),
            )
            .await
            .unwrap();
        assert_eq!(neighbors.len(), 3);
        for n in &neighbors {
            let entity = runtime
                .entities(&token)
                .unwrap()
                .get_entity(n.node_id)
                .await
                .unwrap()
                .expect("target minted");
            assert_eq!(
                entity.entity_type.as_deref(),
                Some("resource"),
                "unfetched target starts as resource"
            );
            assert_eq!(entity.properties.unwrap()["status"], Value::Null);
        }
    }

    // extract(text) mints a derived_from resource holding tag-stripped
    // text, and repeating the call converges on the same id.
    #[tokio::test]
    async fn extract_text_mints_derived_from_resource_idempotent_id() {
        let (runtime, token, _dir) = test_runtime().await;
        let html = b"<html><body><p>Hello   world</p><script>ignored();</script></body></html>";
        let page_id = seed_page(
            &runtime,
            &token,
            "https://origin.example.test/page",
            "text/html",
            html,
        )
        .await;

        let reply1 = run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(page_id),
                url: None,
                kinds: Some(vec!["text".to_string()]),
                namespace: None,
            },
        )
        .await
        .unwrap();
        let text_id_1 = reply1["result"]["text"]["id"].as_str().unwrap().to_string();

        let reply2 = run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(page_id),
                url: None,
                kinds: Some(vec!["text".to_string()]),
                namespace: None,
            },
        )
        .await
        .unwrap();
        let text_id_2 = reply2["result"]["text"]["id"].as_str().unwrap().to_string();
        assert_eq!(
            text_id_1, text_id_2,
            "repeated extraction converges on one id"
        );

        let entity = runtime
            .entities(&token)
            .unwrap()
            .get_entity(uuid::Uuid::parse_str(&text_id_1).unwrap())
            .await
            .unwrap()
            .unwrap();
        let store = crate::blob_store(&runtime).unwrap();
        let content_ref = khive_storage::ContentRef::from_hex(
            entity.properties.unwrap()["blob_ref"].as_str().unwrap(),
        )
        .unwrap();
        let bytes = store
            .get_bounded_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
            .await
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("Hello world"), "{text:?}");
        assert!(
            !text.contains("ignored"),
            "script content is stripped like any other tag body"
        );

        let neighbors = runtime
            .neighbors(
                &token,
                uuid::Uuid::parse_str(&text_id_1).unwrap(),
                khive_storage::Direction::Out,
                None,
                Some(vec![EdgeRelation::DerivedFrom]),
            )
            .await
            .unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].node_id, page_id);
    }

    // extract(text)'s excerpt cap is a BYTE bound, and the cut never
    // splits a multi-byte character even when the raw byte offset lands
    // mid-character.
    #[tokio::test]
    async fn extract_text_caps_excerpt_at_bytes_not_chars_and_never_splits_a_character() {
        let (runtime, token, _dir) = test_runtime().await;
        // Every character below is 2 bytes (`\u{00e9}`) except a single
        // 1-byte `x` prefix, chosen so a raw cut at exactly
        // `MAX_TEXT_EXCERPT_BYTES` (200_000, even) lands in the middle of
        // a character: the prefix shifts every character's start to an
        // odd byte offset, so offset 200_000 sits inside the character at
        // byte range [199_999, 200_001) rather than on a boundary. A
        // truncation that slices without walking back to a char boundary
        // panics on this input; one that truncates by `.chars().take(N)`
        // instead of bytes would let roughly twice the intended byte
        // budget through (every char here is 2 bytes) and fail the
        // length assertion below.
        let content = format!("x{}", "\u{00e9}".repeat(150_000));
        let html = format!("<html><body><p>{content}</p></body></html>");
        let page_id = seed_page(
            &runtime,
            &token,
            "https://origin.example.test/big-multibyte",
            "text/html",
            html.as_bytes(),
        )
        .await;

        let text_id = extract_text(
            &runtime,
            &token,
            page_id,
            "https://origin.example.test/big-multibyte",
            &html,
        )
        .await
        .expect("extract_text does not panic on a non-boundary byte cut");

        let entity = runtime
            .entities(&token)
            .unwrap()
            .get_entity(text_id)
            .await
            .unwrap()
            .unwrap();
        let store = crate::blob_store(&runtime).unwrap();
        let content_ref = khive_storage::ContentRef::from_hex(
            entity.properties.unwrap()["blob_ref"].as_str().unwrap(),
        )
        .unwrap();
        let excerpt_bytes = store
            .get_bounded_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
            .await
            .unwrap();

        assert!(
            excerpt_bytes.len() <= MAX_TEXT_EXCERPT_BYTES,
            "excerpt must respect the byte cap even for an all-multibyte document, got {}",
            excerpt_bytes.len()
        );
        assert_eq!(
            excerpt_bytes.len(),
            199_999,
            "cut walks back exactly one byte from the mid-character offset to the nearest char boundary"
        );
        assert!(
            String::from_utf8(excerpt_bytes).is_ok(),
            "truncation must never split a multi-byte character"
        );
    }

    // extract on a document with no stored body refuses `not_fetched`.
    #[tokio::test]
    async fn extract_on_unfetched_document_refuses_not_fetched() {
        let (runtime, token, _dir) = test_runtime().await;
        let url = Url::parse("https://origin.example.test/never-fetched").unwrap();
        let canonical = identity::canonicalize(url);
        let site = identity::site_id(&canonical);
        let id = identity::document_id(site, &identity::path_and_query(&canonical));
        crate::entities::get_or_create(
            &runtime,
            &token,
            id,
            "document",
            "resource",
            canonical.as_ref(),
            json!({ "url": canonical.to_string(), "status": Value::Null }),
        )
        .await
        .unwrap();

        let err = run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(id),
                url: None,
                kinds: None,
                namespace: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not_fetched"), "{err}");
    }
}
