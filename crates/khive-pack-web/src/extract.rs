//! `web.extract` (ADR-191 D3, D2).
//!
//! Parses an already-fetched body (never fetches one itself — that is
//! `web.fetch`'s job) into whichever of `text`/`links`/`sitemap`/`feed` the
//! caller names, default all applicable to the stored content-type. No HTML
//! or XML parser crate is a workspace dependency, so every extraction here
//! uses regex matches and a fixed-capacity text scan rather than a DOM walk.
//!
//! - `links`: up to the per-page limit of `<a href="...">` values in an HTML
//!   body become `page links_to page|resource` edges to a target
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

use std::borrow::Cow;
use std::sync::{Arc, LazyLock};

use khive_runtime::{KhiveRuntime, LinkSpec, NamespaceToken, RuntimeError};
use khive_storage::EdgeRelation;
use regex::Regex;
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;

use crate::egress::{self, Refusal};
use crate::identity;
use crate::vocab::ExtractParams;
use crate::WebPack;

const MAX_TEXT_EXCERPT_BYTES: usize = 200_000;
pub(crate) const DEFAULT_LINK_LIMIT: u32 = 100;
const MAX_LINK_LIMIT: u32 = 1_000;
// Two fixed text buffers plus at most three UTF-8 bytes per raw byte (U+FFFD).
// This pack-local aggregate budget is separate from raw blob admission. A
// request acquires it once, after hydration, and never upgrades its reservation.
// URL/graph allocations and regex engine scratch are not part of this budget.
const TEXT_SCRATCH_BYTES: usize = 2 * MAX_TEXT_EXCERPT_BYTES;
const MAX_DERIVED_BYTES: usize =
    3 * khive_storage::MAX_BLOB_WHOLE_BYTES as usize + TEXT_SCRATCH_BYTES;
static DERIVED_ADMISSION: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_DERIVED_BYTES)));

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
// Match only a fixed-size, decoded prefix at the current '<'. These retain
// regex's Unicode case folding and word boundary without searching the whole
// document before the first prose character can enter the excerpt.
static SCRIPT_STYLE_START_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^<(?:(script)\b|(style)\b)").expect("valid regex"));
static SCRIPT_STYLE_END_RE: LazyLock<[Regex; 2]> = LazyLock::new(|| {
    [
        Regex::new(r"(?i)^</script>").expect("valid regex"),
        Regex::new(r"(?i)^</style>").expect("valid regex"),
    ]
});

fn decoded_body_bytes(raw: &[u8]) -> usize {
    raw.utf8_chunks()
        .map(|chunk| chunk.valid().len() + usize::from(!chunk.invalid().is_empty()) * 3)
        .sum()
}

async fn admit_derived_buffers(
    admission: &Arc<tokio::sync::Semaphore>,
    decoded_bytes: usize,
) -> Result<tokio::sync::OwnedSemaphorePermit, RuntimeError> {
    let required = decoded_bytes
        .checked_add(TEXT_SCRATCH_BYTES)
        .filter(|required| *required <= MAX_DERIVED_BYTES)
        .ok_or_else(|| {
            RuntimeError::InvalidInput("web.extract: derived buffer bound exceeded".into())
        })?;
    khive_storage::await_request_read_phase(
        "web_extract_derived_admission",
        Arc::clone(admission).acquire_many_owned(required as u32),
    )
    .await?
    .map_err(|error| {
        RuntimeError::Internal(format!("web.extract: derived admission closed: {error}"))
    })
}

fn decode_body(raw: &[u8], decoded_bytes: usize) -> String {
    // A fixed-length allocation avoids String's geometric growth during lossy
    // decoding. The admitted size includes replacement characters, not just raw
    // bytes. No second full-size String is constructed.
    let mut bytes = vec![0; decoded_bytes];
    let mut offset = 0;
    for chunk in raw.utf8_chunks() {
        let valid = chunk.valid().as_bytes();
        bytes[offset..offset + valid.len()].copy_from_slice(valid);
        offset += valid.len();
        if !chunk.invalid().is_empty() {
            bytes[offset..offset + 3].copy_from_slice("\u{fffd}".as_bytes());
            offset += 3;
        }
    }
    String::from_utf8(bytes).expect("UTF-8 chunks and replacement characters are valid")
}

struct TextBuffer {
    bytes: Box<[u8]>,
    len: usize,
    space: bool,
    full: bool,
}

impl TextBuffer {
    fn new() -> Self {
        Self {
            bytes: vec![0; MAX_TEXT_EXCERPT_BYTES].into_boxed_slice(),
            len: 0,
            space: false,
            full: false,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
        self.space = false;
        self.full = false;
    }

    fn push(&mut self, ch: char) {
        if self.full {
            return;
        }
        if ch.is_whitespace() {
            self.space = self.len != 0;
            return;
        }
        if self.space {
            if self.len == self.bytes.len() {
                self.full = true;
                return;
            }
            self.bytes[self.len] = b' ';
            self.len += 1;
            self.space = false;
        }
        let mut encoded = [0; 4];
        let encoded = ch.encode_utf8(&mut encoded).as_bytes();
        if self.len + encoded.len() > self.bytes.len() {
            self.full = true;
            return;
        }
        self.bytes[self.len..self.len + encoded.len()].copy_from_slice(encoded);
        self.len += encoded.len();
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).expect("only complete UTF-8 chars are stored")
    }

    fn saturated(&self) -> bool {
        self.full || self.len == self.bytes.len()
    }

    fn into_string(self) -> String {
        let mut bytes = self.bytes.into_vec();
        bytes.truncate(self.len);
        String::from_utf8(bytes).expect("only complete UTF-8 chars are stored")
    }
}

struct TextScanner<'a> {
    raw: &'a [u8],
    output: TextBuffer,
    // Keep an unclosed '<...' candidate bounded too: the old <[^>]+> strip
    // preserves it as text if no closing '>' exists. Normalizing this candidate
    // as it arrives avoids buffering an arbitrarily long unterminated tag.
    pending: TextBuffer,
    in_tag: bool,
    tag_content: bool,
    no_tag_end: bool,
    no_script_style_end: [bool; 2],
    #[cfg(test)]
    inspected_bytes: usize,
    #[cfg(test)]
    furthest_byte: usize,
}

impl<'a> TextScanner<'a> {
    fn new(raw: &'a [u8]) -> Self {
        Self {
            raw,
            output: TextBuffer::new(),
            pending: TextBuffer::new(),
            in_tag: false,
            tag_content: false,
            no_tag_end: false,
            no_script_style_end: [false; 2],
            #[cfg(test)]
            inspected_bytes: 0,
            #[cfg(test)]
            furthest_byte: 0,
        }
    }

    // Count the ranges actually inspected by decoding/search, including
    // lookahead and repeated work. Tests assert a work bound, not elapsed time.
    fn observe(&mut self, _start: usize, _len: usize) {
        #[cfg(test)]
        {
            self.inspected_bytes += _len;
            self.furthest_byte = self.furthest_byte.max(_start + _len);
        }
    }

    fn next_char(&mut self, offset: &mut usize) -> Option<char> {
        let first = *self.raw.get(*offset)?;
        self.observe(*offset, 1);
        if first.is_ascii() {
            *offset += 1;
            return Some(char::from(first));
        }
        // UTF-8 needs at most four bytes to decide one character or invalid
        // sequence. Calling utf8_chunks()/from_utf8() on the entire remaining
        // body would first scan its entire valid prefix, defeating early exit.
        let end = self.raw.len().min(*offset + 4);
        self.observe(*offset + 1, end - *offset - 1);
        let bytes = &self.raw[*offset..end];
        let ch = match std::str::from_utf8(bytes) {
            Ok(valid) => valid.chars().next().expect("nonempty prefix"),
            Err(error) if error.valid_up_to() != 0 => {
                std::str::from_utf8(&bytes[..error.valid_up_to()])
                    .expect("validated prefix")
                    .chars()
                    .next()
                    .expect("nonempty valid prefix")
            }
            Err(error) => {
                *offset += error.error_len().unwrap_or(bytes.len());
                return Some('\u{fffd}');
            }
        };
        *offset += ch.len_utf8();
        Some(ch)
    }

    fn prefix(&mut self, mut offset: usize) -> ([u8; 36], usize) {
        // Nine characters cover </script> and the word-boundary lookahead for
        // either opening name, including Unicode case-folded spellings.
        let mut prefix = [0; 36];
        let mut len = 0;
        for _ in 0..9 {
            let Some(ch) = self.next_char(&mut offset) else {
                break;
            };
            len += ch.encode_utf8(&mut prefix[len..]).len();
        }
        (prefix, len)
    }

    fn find_byte(&mut self, start: usize, byte: u8) -> Option<usize> {
        let found = self.raw[start..].iter().position(|ch| *ch == byte);
        self.observe(start, found.map_or(self.raw.len() - start, |n| n + 1));
        found.map(|n| start + n)
    }

    fn closed_script_style_end(&mut self, start: usize) -> Option<usize> {
        if self.no_tag_end {
            return None;
        }
        let (prefix, len) = self.prefix(start);
        let prefix = std::str::from_utf8(&prefix[..len]).expect("decoded prefix");
        let captures = SCRIPT_STYLE_START_RE.captures(prefix)?;
        let kind = usize::from(captures.get(1).is_none());
        if self.no_script_style_end[kind] {
            return None;
        }
        let Some(header_end) = self.find_byte(start, b'>') else {
            self.no_tag_end = true;
            return None;
        };
        let mut offset = header_end + 1;
        while let Some(candidate) = self.find_byte(offset, b'<') {
            let (prefix, len) = self.prefix(candidate);
            let prefix = std::str::from_utf8(&prefix[..len]).expect("decoded prefix");
            if let Some(end) = SCRIPT_STYLE_END_RE[kind].find(prefix) {
                // The matched token itself is valid UTF-8, so its byte length
                // is identical in the raw body and the decoded prefix.
                return Some(candidate + end.end());
            }
            offset = candidate + 1;
        }
        // Preserve unterminated scripts as tag-stripped text. Remember the
        // absent closing token so repeated openers do not rescan the same tail
        // quadratically. Every later opener's first '>' is >= header_end.
        self.no_script_style_end[kind] = true;
        None
    }

    fn consume(&mut self, ch: char) {
        if self.in_tag {
            if ch == '>' {
                if self.tag_content {
                    self.output.push(' ');
                } else {
                    self.output.push('<');
                    self.output.push('>');
                }
                self.in_tag = false;
                self.pending.clear();
            } else {
                self.tag_content = true;
                self.pending.push(ch);
            }
        } else if ch == '<' {
            self.in_tag = true;
            self.tag_content = false;
            self.pending.push(ch);
        } else {
            self.output.push(ch);
        }
    }

    fn scan(&mut self) {
        let mut offset = 0;
        while !self.output.saturated() {
            let start = offset;
            let Some(ch) = self.next_char(&mut offset) else {
                break;
            };
            if ch == '<' {
                if let Some(end) = self.closed_script_style_end(start) {
                    offset = end;
                    self.consume(' ');
                    continue;
                }
            }
            self.consume(ch);
        }
        if self.in_tag {
            for ch in self.pending.as_str().chars() {
                if self.output.saturated() {
                    break;
                }
                self.output.push(ch);
            }
        }
    }
}

fn text_excerpt(raw: &[u8]) -> String {
    let mut scanner = TextScanner::new(raw);
    scanner.scan();
    scanner.output.into_string()
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
    crate::entities::require_entity_namespace(token, &entity)?;
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
    link_limit: u32,
) -> Result<(u32, u32), RuntimeError> {
    let mut seen = std::collections::HashSet::new();
    let mut targets = Vec::new();
    let mut skipped = 0u32;
    for capture in HREF_RE.captures_iter(body) {
        if targets.len() >= link_limit as usize {
            skipped += 1;
            continue;
        }
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
        let target_id = identity::document_id(site, &identity::path_and_query(&canonical));
        targets.push((canonical, site, target_id));
    }

    let processed = targets.len() as u32;
    let mut link_specs = Vec::with_capacity(targets.len() * 2);
    for (canonical, site, target_id) in targets {
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
        link_specs.push(LinkSpec {
            namespace: None,
            source_id: site,
            target_id,
            relation: EdgeRelation::Contains,
            weight: 1.0,
            metadata: None,
            resurrect: false,
        });
        link_specs.push(LinkSpec {
            namespace: None,
            source_id: document_id,
            target_id,
            relation: EdgeRelation::LinksTo,
            weight: 1.0,
            metadata: None,
            resurrect: false,
        });
    }
    runtime.link_many(token, link_specs).await?;
    Ok((processed, skipped))
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
    body: &[u8],
    derived_admission: &Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Uuid, RuntimeError> {
    let excerpt = text_excerpt(body);
    let excerpt_bytes = excerpt.len();

    let store = crate::blob_store(runtime)?;
    let content_ref = put_excerpt(store, excerpt, Arc::clone(derived_admission)).await?;

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
            "size": excerpt_bytes as u64,
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

async fn put_excerpt(
    store: Arc<dyn khive_storage::BlobStore>,
    excerpt: String,
    admission: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Result<khive_storage::ContentRef, RuntimeError> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    khive_runtime::track_named_background_task("web_extract_text", async move {
        // Store implementations may move the output buffer into blocking I/O.
        // Keep its reservation until that future completes, even if its request
        // has been cancelled and no longer receives the put result.
        let _admission = admission;
        let result = store.put(excerpt.into_bytes()).await;
        let _ = sender.send(result);
    });
    khive_storage::await_request_read_phase("web_extract_text_put", receiver)
        .await?
        .map_err(|_| RuntimeError::Internal("web.extract: text put supervisor ended".into()))?
        .map_err(RuntimeError::from)
}

async fn run_extract_with_link_selection(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: ExtractParams,
    include_links: bool,
) -> Result<Value, RuntimeError> {
    let link_limit = egress::check_ceiling(
        params.link_limit.map(u64::from),
        u64::from(DEFAULT_LINK_LIMIT),
        u64::from(MAX_LINK_LIMIT),
        "link_limit",
    )? as u32;
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
    let hydrator = runtime.blob_hydrator().ok_or_else(|| {
        RuntimeError::Unconfigured(
            "no BlobStore installed on this server (configure [storage.blob] in khive.toml, or KHIVE_BLOB_ROOT)"
                .to_string(),
        )
    })?;
    let verified = hydrator
        .hydrate_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await?;
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

    let mut kinds: Vec<String> = match params.kinds {
        Some(k) if !k.is_empty() => k,
        _ => applicable_kinds(entity_type, content_type)
            .into_iter()
            .map(str::to_string)
            .collect(),
    };
    if !include_links {
        kinds.retain(|kind| kind != "links");
    }
    for kind in &kinds {
        if !ALL_KINDS.contains(&kind.as_str()) {
            return Err(RuntimeError::InvalidInput(format!(
                "web.extract: unknown kind {kind:?}; expected one of {ALL_KINDS:?}"
            )));
        }
    }

    // Text consumes raw UTF-8 lazily, including replacement characters. Only
    // the other extraction kinds need a fully decoded body; a text-only
    // request must neither validate nor allocate an unused decoded tail.
    let valid_body = if kinds.iter().any(|kind| kind != "text") {
        std::str::from_utf8(verified.bytes()).ok()
    } else {
        Some("")
    };
    let decoded_bytes = if valid_body.is_some() {
        0
    } else {
        decoded_body_bytes(verified.bytes())
    };
    // Raw admission is held first, then derived admission. No path holding a
    // derived permit acquires raw admission again, so these two budgets cannot
    // form a wait cycle. Queued cancellation drops the request's RAII leases
    // before allocating derived buffers. An already-started text put retains a
    // shared derived lease until its background I/O actually completes.
    let derived = Arc::new(admit_derived_buffers(&DERIVED_ADMISSION, decoded_bytes).await?);
    let body = match valid_body {
        Some(body) => Cow::Borrowed(body),
        None => Cow::Owned(decode_body(verified.bytes(), decoded_bytes)),
    };

    let mut result = serde_json::Map::new();
    for kind in &kinds {
        match kind.as_str() {
            "links" => {
                let (count, skipped) =
                    extract_links(runtime, token, target_id, &base_url, &body, link_limit).await?;
                result.insert(
                    "links".to_string(),
                    json!({ "edges_created": count, "skipped": skipped }),
                );
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
                let text_id = extract_text(
                    runtime,
                    token,
                    target_id,
                    &url_str,
                    verified.bytes(),
                    &derived,
                )
                .await?;
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

#[cfg(test)]
async fn run_extract(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: ExtractParams,
) -> Result<Value, RuntimeError> {
    run_extract_with_link_selection(runtime, token, params, true).await
}

impl WebPack {
    pub(crate) async fn handle_extract(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        self.handle_extract_with_link_selection(token, params, true)
            .await
    }

    pub(crate) async fn handle_extract_without_links(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        self.handle_extract_with_link_selection(token, params, false)
            .await
    }

    async fn handle_extract_with_link_selection(
        &self,
        token: &NamespaceToken,
        params: Value,
        include_links: bool,
    ) -> Result<Value, RuntimeError> {
        let params: ExtractParams = serde_json::from_value(params).map_err(|error| {
            RuntimeError::InvalidInput(format!("invalid web.extract arguments: {error}"))
        })?;
        let effective_token =
            crate::namespace::resolve_effective_token(token, params.namespace.as_deref())?;
        run_extract_with_link_selection(&self.runtime, &effective_token, params, include_links)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_pack_kg::KgPack;
    use khive_runtime::VerbRegistryBuilder;
    use khive_types::Namespace;
    use std::sync::Arc;

    mod hydration;

    #[test]
    fn lossy_decode_reserves_replacement_bytes_without_capacity_growth() {
        for raw in [
            b"abc".as_slice(),
            b"\xff\xff\xff",
            b"a\xf0\x90\x80z\xc2",
            b"",
        ] {
            let required = decoded_body_bytes(raw);
            let decoded = decode_body(raw, required);
            assert_eq!(decoded, String::from_utf8_lossy(raw));
            assert_eq!(decoded.len(), required);
            assert_eq!(decoded.capacity(), required);
            assert!(required <= 3 * raw.len());
        }
        // Must fail if admission counts raw bytes instead of replacement bytes.
        assert_eq!(decoded_body_bytes(b"\xff\xff\xff"), 9);
    }

    #[test]
    fn bounded_text_scanner_preserves_tag_and_whitespace_behavior() {
        let scripts =
            Regex::new(r"(?is)<script\b[^>]*>.*?</script>|<style\b[^>]*>.*?</style>").unwrap();
        let tags = Regex::new(r"(?s)<[^>]+>").unwrap();
        let whitespace = Regex::new(r"\s+").unwrap();
        for body in [
            "  Hello\u{2003} world <br> next  ",
            "before<script>secret <b>text</b></script><style>hidden</style>after",
            "<p title='<script>secret</script>'>visible</p>",
            "literal <> <<> end <unfinished\n  tag",
            "<script>unterminated script",
            "<sCrIpT data='>'>ignored</ScRiPt>after",
            "<ſcript>ignored</ſcript><ſtyle>hidden</ſtyle>after",
            "<scripté>prose</script><script\u{301}>also prose</script>",
            "<script/>ignored</script><style!>hidden</style>after",
            "<script><style>hidden</style>visible",
            "<script <style>first</style>second</script>after",
            "\u{0085}é\u{2028}字\u{3000}",
        ] {
            let no_script = scripts.replace_all(body, " ");
            let no_tags = tags.replace_all(&no_script, " ");
            let expected = whitespace.replace_all(no_tags.trim(), " ");
            let actual = text_excerpt(body.as_bytes());
            assert_eq!(actual, expected, "{body:?}");
            assert_eq!(actual.capacity(), MAX_TEXT_EXCERPT_BYTES);
        }
        // A raw candidate buffer capped before whitespace normalization would
        // lose the terminal 'z'; it must remain visible for an unclosed tag.
        let body = format!("<{}z", " ".repeat(2 * MAX_TEXT_EXCERPT_BYTES));
        assert_eq!(text_excerpt(body.as_bytes()), "< z");
        let body = format!("x{}", "é".repeat(MAX_TEXT_EXCERPT_BYTES));
        let excerpt = text_excerpt(body.as_bytes());
        assert_eq!(excerpt.len(), MAX_TEXT_EXCERPT_BYTES - 1);
        assert_eq!(excerpt.capacity(), MAX_TEXT_EXCERPT_BYTES);
    }

    #[test]
    fn bounded_text_scanner_matches_legacy_pipeline_for_malformed_utf8() {
        let scripts =
            Regex::new(r"(?is)<script\b[^>]*>.*?</script>|<style\b[^>]*>.*?</style>").unwrap();
        let tags = Regex::new(r"(?s)<[^>]+>").unwrap();
        let whitespace = Regex::new(r"\s+").unwrap();
        let compare = |raw: &[u8]| {
            let body = String::from_utf8_lossy(raw);
            let no_script = scripts.replace_all(&body, " ");
            let no_tags = tags.replace_all(&no_script, " ");
            let expected = whitespace.replace_all(no_tags.trim(), " ");
            assert_eq!(text_excerpt(raw), expected, "{raw:?}");
        };
        // Exercise every leading byte, incomplete valid sequences, invalid
        // continuation/overlong sequences and a replacement at the script-name
        // word boundary. These use the former full-decode/full-regex pipeline
        // as an independent semantic oracle on deliberately small inputs.
        for byte in 0..=u8::MAX {
            compare(&[b'<', b'p', b'>', byte, b'a', b'<', b'/', b'p', b'>']);
        }
        for raw in [
            b"\xf0\x90\x80z\xc2".as_slice(),
            b"\xf0\x90\x80\x80\xed\xa0\x80\xf4\x90\x80\x80",
            b"<script\xff>hidden</script>after",
            b"<style\xf0\x90>hidden</style>after",
            b"<p title='<script>\xff</script>'>visible</p><\xff tail",
        ] {
            for end in 0..=raw.len() {
                compare(&raw[..end]);
            }
        }
    }

    #[test]
    fn bounded_text_scanner_stops_before_unused_tail() {
        for (prefix, expected) in [
            (
                vec![b'x'; MAX_TEXT_EXCERPT_BYTES],
                "x".repeat(MAX_TEXT_EXCERPT_BYTES),
            ),
            (vec![0xff; 66_667], "\u{fffd}".repeat(66_666)),
        ] {
            let prefix_len = prefix.len();
            let mut raw = b"<p>".to_vec();
            raw.extend_from_slice(&prefix);
            raw.extend_from_slice(b"</p><script>");
            raw.extend(std::iter::repeat_n(b'x', 4 * 1024 * 1024));
            raw.extend_from_slice(b"</script>\xff");
            let mut scanner = TextScanner::new(&raw);
            scanner.scan();
            assert_eq!(scanner.output.as_str(), expected);
            // Must fail if scanning continues after saturation. In particular,
            // neither the trailing script nor the unused malformed byte can
            // cause a full-body regex/UTF-8 pass before taking this excerpt.
            assert!(
                scanner.furthest_byte <= prefix_len + 16,
                "{}",
                scanner.furthest_byte
            );
            assert!(
                scanner.inspected_bytes <= 4 * prefix_len + 64,
                "{}",
                scanner.inspected_bytes
            );
        }
    }

    #[test]
    fn bounded_text_scanner_does_not_rescan_unclosed_script_tails() {
        let body = format!("{}last", "<script><style>".repeat(256));
        let mut scanner = TextScanner::new(body.as_bytes());
        scanner.scan();
        assert_eq!(scanner.output.as_str(), "last");
        // Missing end tags require looking to EOF for legacy semantics, but
        // one remembered failed search per kind keeps repeated openers linear.
        // Must fail if the absent-closing-token cache is removed.
        assert!(
            scanner.inspected_bytes <= 8 * body.len(),
            "{}",
            scanner.inspected_bytes
        );
    }

    #[tokio::test]
    async fn derived_admission_bounds_cancelled_waiters_and_releases_completed_leases() {
        use std::future::{poll_fn, Future};
        use std::task::Poll;

        assert_eq!(MAX_DERIVED_BYTES, 201_726_592);
        let admission = Arc::new(tokio::sync::Semaphore::new(MAX_DERIVED_BYTES));
        let maximum_decode = 3 * khive_storage::MAX_BLOB_WHOLE_BYTES as usize;
        let lease = admit_derived_buffers(&admission, maximum_decode)
            .await
            .unwrap();
        assert_eq!(admission.available_permits(), 0);
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let waiting = khive_storage::scope_request_read_cancellation(
            cancelled,
            admit_derived_buffers(&admission, 0),
        );
        tokio::pin!(waiting);
        // Must fail if admission is bypassed or its lease is released before
        // parsing/persistence. One explicit poll proves blocking, without timing.
        poll_fn(|cx| {
            assert!(waiting.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        cancel.send(true).unwrap();
        let error = waiting.await.unwrap_err();
        assert!(matches!(error,
            RuntimeError::Storage(khive_storage::StorageError::Timeout { ref operation })
            if operation == "web_extract_derived_admission"));
        assert_eq!(admission.available_permits(), 0);
        drop(lease);
        assert_eq!(admission.available_permits(), MAX_DERIVED_BYTES);

        let lease = admit_derived_buffers(&admission, 0).await.unwrap();
        assert_eq!(
            admission.available_permits(),
            MAX_DERIVED_BYTES - TEXT_SCRATCH_BYTES
        );
        drop(lease);
        assert!(admit_derived_buffers(&admission, maximum_decode + 1)
            .await
            .is_err());
        assert_eq!(admission.available_permits(), MAX_DERIVED_BYTES);
    }

    #[derive(Debug)]
    struct PausedPut {
        inner: Arc<dyn khive_storage::BlobStore>,
        started: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
    }

    #[async_trait::async_trait]
    impl khive_storage::BlobStore for PausedPut {
        async fn put(
            &self,
            bytes: Vec<u8>,
        ) -> khive_storage::StorageResult<khive_storage::ContentRef> {
            self.started.notify_one();
            self.release.acquire().await.unwrap().forget();
            self.inner.put(bytes).await
        }

        async fn get_bounded_verified(
            &self,
            id: &khive_storage::ContentRef,
            max: u64,
        ) -> khive_storage::StorageResult<Vec<u8>> {
            self.inner.get_bounded_verified(id, max).await
        }

        async fn exists(
            &self,
            id: &khive_storage::ContentRef,
        ) -> khive_storage::StorageResult<bool> {
            self.inner.exists(id).await
        }

        async fn size(
            &self,
            id: &khive_storage::ContentRef,
        ) -> khive_storage::StorageResult<Option<u64>> {
            self.inner.size(id).await
        }

        async fn delete(
            &self,
            id: &khive_storage::ContentRef,
        ) -> khive_storage::StorageResult<bool> {
            self.inner.delete(id).await
        }
    }

    #[tokio::test]
    async fn cancelled_text_put_keeps_admission_until_background_io_finishes() {
        use std::future::{poll_fn, Future};
        use std::task::Poll;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(PausedPut {
            inner: Arc::new(
                khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0).unwrap(),
            ),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        });
        let admission = Arc::new(tokio::sync::Semaphore::new(TEXT_SCRATCH_BYTES));
        let permit = Arc::new(admit_derived_buffers(&admission, 0).await.unwrap());
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let writing = khive_storage::scope_request_read_cancellation(
            cancelled,
            put_excerpt(store.clone(), "excerpt".into(), permit),
        );
        tokio::pin!(writing);
        // The notification is the assertion boundary; the timeout only catches
        // a hung test. No elapsed-time assumption decides admission correctness.
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut writing => panic!("put missed the barrier: {result:?}"),
                _ = store.started.notified() => {},
            }
        })
        .await
        .unwrap();
        cancel.send(true).unwrap();
        let error = writing.await.unwrap_err();
        assert!(matches!(error,
            RuntimeError::Storage(khive_storage::StorageError::Timeout { ref operation })
            if operation == "web_extract_text_put"));
        // Must fail if put is awaited in the cancelled request without a
        // supervisor retaining the derived lease alongside its owned bytes.
        assert_eq!(admission.available_permits(), 0);
        let next = admit_derived_buffers(&admission, 0);
        tokio::pin!(next);
        poll_fn(|cx| {
            assert!(next.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        store.release.add_permits(1);
        let next_lease = tokio::time::timeout(Duration::from_secs(5), &mut next)
            .await
            .unwrap()
            .unwrap();
        drop(next_lease);
        assert_eq!(admission.available_permits(), TEXT_SCRATCH_BYTES);
        let content_ref =
            khive_storage::ContentRef::from_digest_bytes(blake3::hash(b"excerpt").as_bytes());
        assert!(store.inner.exists(&content_ref).await.unwrap());
    }

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
                link_limit: None,
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
                link_limit: None,
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
                link_limit: None,
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

        let derived = Arc::new(admit_derived_buffers(&DERIVED_ADMISSION, 0).await.unwrap());
        let text_id = extract_text(
            &runtime,
            &token,
            page_id,
            "https://origin.example.test/big-multibyte",
            html.as_bytes(),
            &derived,
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
                link_limit: None,
                namespace: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not_fetched"), "{err}");
    }
}
