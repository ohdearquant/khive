//! `web.extract` (ADR-191 D3, D2).
//!
//! Parses an already-fetched body (never fetches one itself — that is
//! `web.fetch`'s job) into whichever of `text`/`links`/`sitemap`/`feed` the
//! caller names, default all applicable to the stored content-type. No HTML
//! or XML parser crate is a workspace dependency, so every extraction here
//! uses regex matches and a fixed-capacity text scan rather than a DOM walk.
//!
//! - `links`: admitted `<a>`, `<link>`, and response `Link` targets become
//!   live `page links_to page|resource` edges with occurrence evidence. An
//!   unfetched target is minted as a `resource` (`status: null`), and a
//!   marked extracted edge is retracted only when its target is absent from
//!   the full parsed set.
//! - `sitemap`/`feed`: admitted `<loc>`/`<link>` entries become a `resource`
//!   under the document's own `site`, linked `site contains resource` (the
//!   pack's second `EDGE_RULES` row) — a feed/sitemap entry is the site's
//!   content, not the feed document's.
//! - `text`: a `resource` holding the tag-stripped text, linked
//!   `document derived_from document` and keyed by the source document and
//!   stored body reference. A new body cannot overwrite an old excerpt.
//!
//! Every successful extraction writes an immutable note and roots its input
//! body there for later reconstruction.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use khive_runtime::{EdgeListFilter, KhiveRuntime, LinkSpec, NamespaceToken, RuntimeError};
use khive_storage::{BlobStore, ContentRef, EdgeRelation, StorageCapability, StorageError};
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
const MAX_LINK_OCCURRENCES: usize = 10_000;
const MAX_LINK_CONTEXT_BYTES: usize = 256;
// Two fixed text buffers plus at most three UTF-8 bytes per raw byte (U+FFFD).
// This pack-local aggregate budget is separate from raw blob admission. A
// request acquires it once, after hydration, and never upgrades its reservation.
// URL/graph allocations and regex engine scratch are not part of this budget.
const TEXT_SCRATCH_BYTES: usize = 2 * MAX_TEXT_EXCERPT_BYTES;
const MAX_DERIVED_BYTES: usize =
    3 * khive_storage::MAX_BLOB_WHOLE_BYTES as usize + TEXT_SCRATCH_BYTES;
static DERIVED_ADMISSION: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_DERIVED_BYTES)));

static HTML_LINK_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<(a|link)\b([^>]*)>").expect("valid regex"));
static HTML_ATTRIBUTE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\b([a-z][a-z0-9:_-]*)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#)
        .expect("valid regex")
});
static HTML_CLOSE_ANCHOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</a\s*>").expect("valid regex"));
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

async fn source_blob_size(
    store: &dyn BlobStore,
    content_ref: &ContentRef,
) -> khive_storage::StorageResult<Option<u64>> {
    khive_storage::await_request_read_phase("web_extract_blob_size", store.size(content_ref))
        .await?
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
            let site = identity::site_id(token.namespace(), &canonical);
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

struct LinkOccurrence {
    source: &'static str,
    href: String,
    rel: Vec<String>,
    context: String,
    context_truncated: bool,
}

struct LinkOccurrenceLimit;

impl LinkOccurrenceLimit {
    fn refusal(&self) -> Value {
        json!({
            "code": "too_many_link_occurrences",
            "reason": format!(
                "web.extract: more than {MAX_LINK_OCCURRENCES} link occurrences; narrow the input"
            ),
            "limit": MAX_LINK_OCCURRENCES,
            "observed_at_least": MAX_LINK_OCCURRENCES + 1,
        })
    }
}

fn link_attribute(attributes: &str, name: &str) -> Option<String> {
    HTML_ATTRIBUTE_RE
        .captures_iter(attributes)
        .find_map(|capture| {
            if !capture[1].eq_ignore_ascii_case(name) {
                return None;
            }
            capture
                .get(2)
                .or_else(|| capture.get(3))
                .or_else(|| capture.get(4))
                .map(|value| value.as_str().to_string())
        })
}

fn relation_tokens(raw: &str) -> Vec<String> {
    raw.split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect()
}

fn link_context(raw: &str) -> (String, bool) {
    let mut context = String::new();
    let mut in_tag = false;
    let mut space = false;
    for ch in raw.chars() {
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch == '>' && in_tag {
            in_tag = false;
            continue;
        }
        if in_tag {
            continue;
        }
        if ch.is_whitespace() {
            space = !context.is_empty();
            continue;
        }
        if space && context.len() < MAX_LINK_CONTEXT_BYTES {
            context.push(' ');
        }
        space = false;
        if context.len() + ch.len_utf8() > MAX_LINK_CONTEXT_BYTES {
            return (context.trim_end().to_string(), true);
        }
        context.push(ch);
    }
    (context.trim_end().to_string(), false)
}

fn push_occurrence(
    occurrences: &mut Vec<LinkOccurrence>,
    occurrence: LinkOccurrence,
) -> Result<(), LinkOccurrenceLimit> {
    if occurrences.len() == MAX_LINK_OCCURRENCES {
        return Err(LinkOccurrenceLimit);
    }
    occurrences.push(occurrence);
    Ok(())
}

fn parse_link_occurrences(
    body: &str,
    link_headers: &[String],
) -> Result<Vec<LinkOccurrence>, LinkOccurrenceLimit> {
    let mut occurrences = Vec::new();
    for tag in HTML_LINK_TAG_RE.captures_iter(body) {
        let attributes = &tag[2];
        let Some(href) = link_attribute(attributes, "href") else {
            continue;
        };
        let source = if tag[1].eq_ignore_ascii_case("a") {
            "anchor"
        } else {
            "link"
        };
        let (context, context_truncated) = if source == "anchor" {
            let after = &body[tag.get(0).expect("whole tag").end()..];
            let mut end = after.len().min(MAX_LINK_CONTEXT_BYTES * 4);
            while !after.is_char_boundary(end) {
                end -= 1;
            }
            let candidate = &after[..end];
            let inner = HTML_CLOSE_ANCHOR_RE
                .find(candidate)
                .map_or(candidate, |close| &candidate[..close.start()]);
            let (text, truncated) = link_context(inner);
            (text, truncated || inner.len() == end && end < after.len())
        } else {
            link_context(&link_attribute(attributes, "title").unwrap_or_default())
        };
        push_occurrence(
            &mut occurrences,
            LinkOccurrence {
                source,
                href,
                rel: relation_tokens(&link_attribute(attributes, "rel").unwrap_or_default()),
                context,
                context_truncated,
            },
        )?;
    }
    for header in link_headers {
        let mut start = 0;
        let mut quoted = false;
        let mut angled = false;
        for (index, ch) in header
            .char_indices()
            .chain(std::iter::once((header.len(), ',')))
        {
            match ch {
                '"' => quoted = !quoted,
                '<' if !quoted => angled = true,
                '>' if !quoted => angled = false,
                ',' if !quoted && !angled => {
                    let entry = header[start..index].trim();
                    if let Some(open) = entry.find('<') {
                        if let Some(close) = entry[open + 1..].find('>') {
                            let close = open + 1 + close;
                            let href = entry[open + 1..close].trim();
                            let attributes = &entry[close + 1..];
                            let mut rel = Vec::new();
                            let mut context = String::new();
                            let mut context_truncated = false;
                            for parameter in attributes.split(';').skip(1) {
                                if let Some((name, value)) = parameter.trim().split_once('=') {
                                    let value = value.trim().trim_matches('"').trim_matches('\'');
                                    if name.trim().eq_ignore_ascii_case("rel") {
                                        rel.extend(relation_tokens(value));
                                    } else if name.trim().eq_ignore_ascii_case("title") {
                                        (context, context_truncated) = link_context(value);
                                    }
                                }
                            }
                            push_occurrence(
                                &mut occurrences,
                                LinkOccurrence {
                                    source: "header",
                                    href: href.to_string(),
                                    rel,
                                    context,
                                    context_truncated,
                                },
                            )?;
                        }
                    }
                    start = index + 1;
                }
                _ => {}
            }
        }
    }
    Ok(occurrences)
}

struct LinkTarget {
    request_url: Url,
    canonical: Url,
    site: Uuid,
    id: Uuid,
    occurrences: Vec<Value>,
}

struct ExistingLinks {
    existing_edge_ids: HashMap<Uuid, Uuid>,
    unmarked_targets: HashSet<Uuid>,
    legacy_claimed_targets: Vec<Uuid>,
    legacy_claimed: Vec<Value>,
    legacy_unclaimed: Vec<Value>,
    snapshots: HashMap<Uuid, khive_storage::Edge>,
    retractions: Vec<khive_storage::Edge>,
}

fn is_legacy_extractor_write(edge: &khive_storage::Edge) -> bool {
    edge.metadata.is_none() && edge.weight == 1.0
}

async fn inspect_existing_links(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    document_id: Uuid,
    present: &HashSet<Uuid>,
    admitted: &HashSet<Uuid>,
) -> Result<ExistingLinks, RuntimeError> {
    let filter = EdgeListFilter {
        source_id: Some(document_id),
        relations: vec![EdgeRelation::LinksTo],
        ..Default::default()
    };
    let mut existing = Vec::new();
    let mut offset = 0;
    loop {
        let page = runtime
            .list_edges(token, filter.clone(), 1_000, offset)
            .await?;
        let count = page.len();
        existing.extend(page);
        if count < 1_000 {
            break;
        }
        offset += count as u32;
    }
    let mut existing_edge_ids = HashMap::new();
    let mut unmarked_targets = HashSet::new();
    let mut legacy_claimed_targets = Vec::new();
    let mut legacy_claimed = Vec::new();
    let mut legacy_unclaimed = Vec::new();
    let mut retractions = Vec::new();
    let mut snapshots = HashMap::new();
    for edge in existing {
        if edge.namespace != token.namespace().as_str() {
            continue;
        }
        let marked = edge
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("web_extract"))
            == Some(&Value::Bool(true));
        snapshots.insert(edge.target_id, edge.clone());
        existing_edge_ids.insert(edge.target_id, Uuid::from(edge.id));
        if !marked {
            if present.contains(&edge.target_id) && is_legacy_extractor_write(&edge) {
                legacy_claimed_targets.push(edge.target_id);
                legacy_claimed.push(json!({
                    "edge_id": Uuid::from(edge.id).to_string(),
                    "target_id": edge.target_id.to_string(),
                    "admitted": admitted.contains(&edge.target_id),
                }));
            } else {
                unmarked_targets.insert(edge.target_id);
                legacy_unclaimed.push(json!({
                    "edge_id": Uuid::from(edge.id).to_string(),
                    "target_id": edge.target_id.to_string(),
                    "live": true,
                    "present_in_extraction": present.contains(&edge.target_id),
                    "collides_with_admitted_target": admitted.contains(&edge.target_id),
                }));
            }
        } else if !present.contains(&edge.target_id) {
            retractions.push(edge);
        }
    }
    // `list_edges` intentionally omits tombstones. An unmarked soft-deleted
    // triple is still a caller-owned natural key and must not be resurrected
    // as extractor-owned merely because its target reappears.
    for target_id in admitted {
        if existing_edge_ids.contains_key(target_id) {
            continue;
        }
        let Some(edge) = runtime
            .get_edge_by_natural_key_including_deleted(
                token,
                token.namespace().as_str(),
                document_id,
                *target_id,
                EdgeRelation::LinksTo,
            )
            .await?
        else {
            continue;
        };
        snapshots.insert(*target_id, edge.clone());
        existing_edge_ids.insert(*target_id, Uuid::from(edge.id));
        let marked = edge
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("web_extract"))
            == Some(&Value::Bool(true));
        if !marked {
            if edge.deleted_at.is_none() && is_legacy_extractor_write(&edge) {
                legacy_claimed_targets.push(*target_id);
                legacy_claimed.push(json!({
                    "edge_id": Uuid::from(edge.id).to_string(),
                    "target_id": target_id.to_string(),
                    "admitted": true,
                }));
            } else {
                unmarked_targets.insert(*target_id);
                legacy_unclaimed.push(json!({
                    "edge_id": Uuid::from(edge.id).to_string(),
                    "target_id": target_id.to_string(),
                    "live": edge.deleted_at.is_none(),
                    "present_in_extraction": true,
                    "collides_with_admitted_target": true,
                }));
            }
        }
    }
    Ok(ExistingLinks {
        existing_edge_ids,
        unmarked_targets,
        legacy_claimed_targets,
        legacy_claimed,
        legacy_unclaimed,
        snapshots,
        retractions,
    })
}

#[allow(clippy::too_many_arguments)]
async fn extract_links(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    document_id: Uuid,
    base_url: &Url,
    occurrences: Vec<LinkOccurrence>,
    link_limit: u32,
    source_content_ref: &str,
    capture_receipt_id: Option<Uuid>,
) -> Result<(u32, u32, u32, Vec<Value>, Vec<Value>, Vec<Value>), RuntimeError> {
    let mut positions: HashMap<Uuid, usize> = HashMap::new();
    let mut unadmitted_positions: HashMap<Uuid, usize> = HashMap::new();
    let mut present = HashSet::new();
    let mut targets: Vec<LinkTarget> = Vec::new();
    let mut unadmitted: Vec<LinkTarget> = Vec::new();
    let mut skipped = 0u32;
    for occurrence in occurrences {
        let href = occurrence.href.trim();
        let disallowed_scheme = ["javascript:", "mailto:"].into_iter().any(|prefix| {
            href.get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        });
        if href.is_empty() || href.starts_with('#') || disallowed_scheme {
            continue;
        }
        let Some(target_url) = resolve_against(base_url, href) else {
            continue;
        };
        let request_url = identity::request_url(target_url);
        let canonical = identity::canonicalize(request_url.clone());
        if canonical.scheme() != "http" && canonical.scheme() != "https" {
            continue;
        }
        let site = identity::site_id(token.namespace(), &canonical);
        let target_id = identity::document_id(site, &identity::path_and_query(&canonical));
        present.insert(target_id);
        let evidence = json!({
            "source": occurrence.source,
            "href": occurrence.href,
            "rel": occurrence.rel,
            "context": occurrence.context,
            "context_truncated": occurrence.context_truncated,
        });
        if let Some(index) = positions.get(&target_id).copied() {
            targets[index].occurrences.push(evidence);
        } else if targets.len() < link_limit as usize {
            positions.insert(target_id, targets.len());
            targets.push(LinkTarget {
                request_url,
                canonical,
                site,
                id: target_id,
                occurrences: vec![evidence],
            });
        } else {
            if let Some(index) = unadmitted_positions.get(&target_id).copied() {
                unadmitted[index].occurrences.push(evidence);
            } else {
                skipped = skipped.saturating_add(1);
                unadmitted_positions.insert(target_id, unadmitted.len());
                unadmitted.push(LinkTarget {
                    request_url,
                    canonical,
                    site,
                    id: target_id,
                    occurrences: vec![evidence],
                });
            }
        }
    }

    let processed = targets.len() as u32;
    let admitted: HashSet<Uuid> = targets.iter().map(|target| target.id).collect();
    // Inspect before link_many: its natural-key upsert would overwrite an
    // unmarked caller edge and make its ownership impossible to recover.
    let existing = inspect_existing_links(runtime, token, document_id, &present, &admitted).await?;
    let mut link_specs = Vec::with_capacity(targets.len() * 2);
    let mut owned_positions = HashMap::new();
    for target in &targets {
        crate::entities::get_or_create(
            runtime,
            token,
            target.site,
            "service",
            "site",
            &identity::site_key(&target.canonical),
            json!({
                "scheme": target.canonical.scheme(),
                "host": target.canonical.host_str(),
                "port": target.canonical.port_or_known_default(),
            }),
        )
        .await?;
        crate::entities::get_or_create(
            runtime,
            token,
            target.id,
            "document",
            "resource",
            target.canonical.as_ref(),
            json!({ "url": target.request_url.to_string(), "status": Value::Null }),
        )
        .await?;
        link_specs.push(LinkSpec {
            namespace: None,
            source_id: target.site,
            target_id: target.id,
            relation: EdgeRelation::Contains,
            weight: 1.0,
            metadata: None,
            resurrect: false,
        });
        if !existing.unmarked_targets.contains(&target.id) {
            owned_positions.insert(target.id, link_specs.len());
            link_specs.push(LinkSpec {
                namespace: None,
                source_id: document_id,
                target_id: target.id,
                relation: EdgeRelation::LinksTo,
                weight: 1.0,
                metadata: Some(json!({
                    "web_extract": true,
                    "source_content_ref": source_content_ref,
                    "capture_receipt_id": capture_receipt_id.map(|id| id.to_string()),
                    "occurrence_count": target.occurrences.len(),
                    "occurrences": target.occurrences,
                })),
                resurrect: true,
            });
        }
    }
    // A present default-shaped legacy edge beyond the admission budget was
    // still observed in this capture. Mark ownership with claim provenance
    // only; the immutable note carries this run's occurrence detail.
    for target_id in &existing.legacy_claimed_targets {
        if admitted.contains(target_id) {
            continue;
        }
        link_specs.push(LinkSpec {
            namespace: None,
            source_id: document_id,
            target_id: *target_id,
            relation: EdgeRelation::LinksTo,
            weight: 1.0,
            metadata: Some(json!({
                "web_extract": true,
                "claimed_source_content_ref": source_content_ref,
                "capture_receipt_id": capture_receipt_id.map(|id| id.to_string()),
            })),
            resurrect: false,
        });
    }
    let guards = link_specs
        .iter()
        .filter(|spec| spec.relation == EdgeRelation::LinksTo)
        .map(|spec| khive_db::stores::graph::GraphEdgeSnapshotGuard {
            namespace: token.namespace().as_str().to_owned(),
            source_id: document_id,
            target_id: spec.target_id,
            relation: EdgeRelation::LinksTo,
            expected: existing.snapshots.get(&spec.target_id).cloned(),
        })
        .collect();
    // Attachment concordance was checked on canonical main before preparation.
    // This source-backend transaction fences the document body property and all
    // selected edge snapshots, including absence, before any reconciliation DML.
    let (rows, _) = runtime
        .link_many_guarded_observed(
            token,
            link_specs,
            khive_db::stores::graph::GraphMutationPreconditions {
                document: Some(khive_db::stores::graph::GraphDocumentGuard {
                    namespace: token.namespace().as_str().to_owned(),
                    id: document_id,
                    expected_blob_ref: source_content_ref.to_owned(),
                }),
                edges: guards,
            },
            existing.retractions.clone(),
        )
        .await?;
    let edges: Vec<_> = rows.into_iter().map(|row| row.edge).collect();
    let collisions = targets
        .iter()
        .filter(|target| existing.unmarked_targets.contains(&target.id))
        .count() as u32;
    let mut evidence: Vec<Value> = targets
        .iter()
        .map(|target| {
            let edge_id = owned_positions
                .get(&target.id)
                .map(|index| Uuid::from(edges[*index].id))
                .or_else(|| existing.existing_edge_ids.get(&target.id).copied());
            json!({
                "target_id": target.id.to_string(),
                "edge_id": edge_id.map(|id| id.to_string()),
                "url": target.canonical.to_string(),
                "occurrence_count": target.occurrences.len(),
                "occurrences": target.occurrences,
                "admitted": true,
                "ownership_collision": existing.unmarked_targets.contains(&target.id),
            })
        })
        .collect();
    evidence.extend(unadmitted.iter().map(|target| {
        json!({
            "target_id": target.id.to_string(),
            "edge_id": existing.existing_edge_ids.get(&target.id).map(Uuid::to_string),
            "url": target.canonical.to_string(),
            "occurrence_count": target.occurrences.len(),
            "occurrences": target.occurrences,
            "admitted": false,
            "ownership_collision": existing.unmarked_targets.contains(&target.id),
        })
    }));
    Ok((
        processed,
        skipped,
        collisions,
        evidence,
        existing.legacy_claimed,
        existing.legacy_unclaimed,
    ))
}

async fn extract_entries(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    site_id: Uuid,
    body: &str,
    kind: &str,
    entry_limit: u32,
) -> Result<(u32, u32), RuntimeError> {
    // Iterate directly over the stored body rather than collecting every
    // remotely supplied entry before the ceiling can be applied.
    let urls: Box<dyn Iterator<Item = String> + Send + '_> = match kind {
        "sitemap" => Box::new(LOC_RE.captures_iter(body).map(|c| c[1].trim().to_string())),
        "feed" => Box::new(
            ATOM_LINK_HREF_RE
                .captures_iter(body)
                .map(|c| c[1].trim().to_string())
                .chain(
                    RSS_LINK_RE
                        .captures_iter(body)
                        .map(|c| c[1].trim().to_string()),
                ),
        ),
        _ => Box::new(std::iter::empty()),
    };
    let mut seen = std::collections::HashSet::new();
    let mut count = 0u32;
    let mut skipped = 0u32;
    let mut link_specs = Vec::with_capacity((entry_limit as usize).saturating_mul(2));
    for raw in urls {
        if count >= entry_limit {
            skipped = skipped.saturating_add(1);
            continue;
        }
        let Ok(url) = Url::parse(&raw) else { continue };
        let request_url = identity::request_url(url);
        let canonical = identity::canonicalize(request_url.clone());
        if canonical.scheme() != "http" && canonical.scheme() != "https" {
            continue;
        }
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let entry_site = identity::site_id(token.namespace(), &canonical);
        let target_id = identity::document_id(entry_site, &identity::path_and_query(&canonical));
        crate::entities::get_or_create(
            runtime,
            token,
            target_id,
            "document",
            "resource",
            canonical.as_ref(),
            json!({ "url": request_url.to_string(), "status": Value::Null }),
        )
        .await?;
        // Entries belong to the SITE that published the feed/sitemap, which
        // is the source document's own site (D2: "site contains resource ...
        // extract (sitemap and feed entries)") — not necessarily the
        // entry's own site when the entry points elsewhere, so both edges
        // are recorded: containment under the publishing site, plus the
        // entry's own site if it differs.
        link_specs.push(LinkSpec {
            namespace: None,
            source_id: site_id,
            target_id,
            relation: EdgeRelation::Contains,
            weight: 1.0,
            metadata: None,
            resurrect: false,
        });
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
            link_specs.push(LinkSpec {
                namespace: None,
                source_id: entry_site,
                target_id,
                relation: EdgeRelation::Contains,
                weight: 1.0,
                metadata: None,
                resurrect: false,
            });
        }
        count += 1;
    }
    runtime.link_many(token, link_specs).await?;
    Ok((count, skipped))
}

#[allow(clippy::too_many_arguments)]
async fn extract_text(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    original_id: Uuid,
    original_url: &str,
    source_content_ref: &str,
    capture_receipt_id: Option<Uuid>,
    body: &[u8],
    derived_admission: &Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Uuid, RuntimeError> {
    let excerpt = text_excerpt(body);
    let excerpt_bytes = excerpt.len();

    let store = crate::blob_store(runtime)?;
    let content_ref = put_excerpt(store, excerpt, Arc::clone(derived_admission)).await?;

    let text_id = identity::derived_text_id(original_id, source_content_ref);
    crate::entities::get_or_create(
        runtime,
        token,
        text_id,
        "document",
        "resource",
        &format!("{original_url} (extracted text)"),
        json!({
            "derived_from": original_id.to_string(),
            "source_content_ref": source_content_ref,
            "capture_receipt_id": capture_receipt_id.map(|id| id.to_string()),
        }),
    )
    .await?;
    crate::entities::patch(
        runtime,
        token,
        text_id,
        Some("resource"),
        json!({
            "derived_from": original_id.to_string(),
            "source_content_ref": source_content_ref,
            "content_type": "text/plain",
            "blob_ref": content_ref.to_string(),
            "size": excerpt_bytes as u64,
        }),
    )
    .await?;
    crate::fetch::root_body(
        runtime,
        text_id,
        khive_storage::AttachmentSubstrate::Entity,
        &content_ref,
        Some("text/plain"),
        excerpt_bytes as u64,
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

async fn verify_source_body(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    document_id: Uuid,
    source_content_ref: &str,
) -> Result<(), RuntimeError> {
    let current = runtime.entities(token)?.get_entity(document_id).await?;
    if let Some(entity) = &current {
        crate::entities::require_entity_namespace(token, entity)?;
    }
    // Web graph rows may live on a routed backend, while all body roots live
    // on canonical main (ADR-191 A1). The graph backend's content_ref
    // projection therefore cannot establish attachment concordance.
    let attachment = runtime
        .core()
        .attachments()?
        .get_attachment(document_id, "content")
        .await?;
    if attachment
        .as_ref()
        .filter(|attachment| attachment.substrate == khive_storage::AttachmentSubstrate::Entity)
        .map(|attachment| attachment.content_ref.as_str())
        != Some(source_content_ref)
        || current
            .as_ref()
            .and_then(|entity| entity.properties.as_ref())
            .and_then(|properties| properties.get("blob_ref"))
            .and_then(Value::as_str)
            != Some(source_content_ref)
    {
        return Err(Refusal::new(
            "capture_changed",
            "the stored body or its content attachment changed while web.extract read it; retry",
        )
        .into());
    }
    Ok(())
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
    let source_content_ref = content_ref.to_string();
    let capture =
        crate::receipt::capture_for_body(runtime, token, &entity, &source_content_ref).await?;
    let capture_receipt_id = capture.as_ref().map(|(id, _)| *id);
    let link_headers: Vec<String> = capture
        .as_ref()
        .and_then(|(_, request)| request["headers"]["link"].as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let hydrator = runtime.blob_hydrator().ok_or_else(|| {
        RuntimeError::Unconfigured(
            "no BlobStore installed on this server (configure [storage.blob] in khive.toml, or KHIVE_BLOB_ROOT)"
                .to_string(),
        )
    })?;
    verify_source_body(runtime, token, target_id, &source_content_ref).await?;
    let blob_store = crate::blob_store(runtime)?;
    let size = match source_blob_size(blob_store.as_ref(), &content_ref).await {
        Ok(Some(size)) => size,
        Ok(None)
        | Err(StorageError::Unsupported {
            capability: StorageCapability::Blob,
            ..
        }) => khive_storage::MAX_BLOB_WHOLE_BYTES,
        Err(error) => return Err(error.into()),
    };
    if size > khive_storage::MAX_BLOB_WHOLE_BYTES {
        return Err(StorageError::BlobTooLarge {
            content_ref,
            max_bytes: khive_storage::MAX_BLOB_WHOLE_BYTES,
            observed_at_least: size,
        }
        .into());
    }
    let verified = hydrator.hydrate_verified(&content_ref, size).await?;
    let url_str = properties
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let base_url = Url::parse(&url_str)
        .map_err(|error| RuntimeError::Internal(format!("stored url is invalid: {error}")))?;
    let canonical = identity::canonicalize(base_url.clone());
    let site_id = crate::fetch::canonical_site(runtime, token, &canonical).await?;
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
    let mut seen = HashSet::new();
    kinds.retain(|kind| seen.insert(kind.clone()));

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

    // Parse before any kind writes. A link occurrence refusal degrades only
    // the links kind; the final extraction note still roots this source body.
    let mut parsed_links = kinds
        .iter()
        .any(|kind| kind == "links")
        .then(|| parse_link_occurrences(&body, &link_headers));
    // The second check closes the hydration and parse window before any
    // derived row, link, or extraction note is written. Link reconciliation
    // also checks the source document property inside its graph transaction;
    // canonical-main attachment concordance remains an outside precheck.
    verify_source_body(runtime, token, target_id, &source_content_ref).await?;
    let mut result = serde_json::Map::new();
    let mut targets_remaining = link_limit;
    let mut link_evidence = Vec::new();
    let mut legacy_claimed = Vec::new();
    let mut legacy_unclaimed = Vec::new();
    let mut link_skipped = 0;
    let mut link_collisions = 0;
    let mut derived_ids = Vec::new();
    let mut refusal = None;
    for kind in &kinds {
        match kind.as_str() {
            "links" => match parsed_links.take().expect("selected links were pre-parsed") {
                Ok(occurrences) => {
                    let (count, skipped, collisions, evidence, claimed, unclaimed) = extract_links(
                        runtime,
                        token,
                        target_id,
                        &base_url,
                        occurrences,
                        targets_remaining,
                        &source_content_ref,
                        capture_receipt_id,
                    )
                    .await?;
                    targets_remaining = targets_remaining.saturating_sub(count);
                    link_evidence = evidence;
                    legacy_claimed = claimed;
                    legacy_unclaimed = unclaimed;
                    link_skipped = skipped;
                    link_collisions = collisions;
                    result.insert(
                        "links".to_string(),
                        json!({
                            "edges_created": count - collisions,
                            "admitted_targets": count,
                            "skipped": skipped,
                            "ownership_collisions": collisions,
                        }),
                    );
                }
                Err(limit) => {
                    let reason = limit.refusal();
                    result.insert(
                        "links".to_string(),
                        json!({ "refused": true, "refusal": reason }),
                    );
                    refusal = Some(reason);
                }
            },
            "sitemap" => {
                let (count, skipped) =
                    extract_entries(runtime, token, site_id, &body, "sitemap", targets_remaining)
                        .await?;
                targets_remaining = targets_remaining.saturating_sub(count);
                result.insert(
                    "sitemap".to_string(),
                    json!({ "entries": count, "skipped": skipped }),
                );
            }
            "feed" => {
                let (count, skipped) =
                    extract_entries(runtime, token, site_id, &body, "feed", targets_remaining)
                        .await?;
                targets_remaining = targets_remaining.saturating_sub(count);
                result.insert(
                    "feed".to_string(),
                    json!({ "entries": count, "skipped": skipped }),
                );
            }
            "text" => {
                let text_id = extract_text(
                    runtime,
                    token,
                    target_id,
                    &url_str,
                    &source_content_ref,
                    capture_receipt_id,
                    verified.bytes(),
                    &derived,
                )
                .await?;
                derived_ids.push(text_id);
                result.insert("text".to_string(), json!({ "id": text_id.to_string() }));
            }
            _ => unreachable!("validated above"),
        }
    }

    let extraction_receipt_id = crate::receipt::write_extraction_receipt(
        runtime,
        token,
        target_id,
        json!({
            "verb": "web.extract",
            "source_content_ref": source_content_ref,
            "capture_receipt_id": capture_receipt_id.map(|id| id.to_string()),
            "kinds": kinds,
            "result": result,
            "links": link_evidence,
            "legacy_claimed": if refusal.is_some() { Value::Null } else { json!(legacy_claimed) },
            "legacy_unclaimed": if refusal.is_some() { Value::Null } else { json!(legacy_unclaimed) },
            "links_complete": refusal.is_none() && link_skipped == 0 && link_collisions == 0,
            "status": if refusal.is_some() { "degraded" } else { "complete" },
            "refusal": refusal,
        }),
        derived_ids,
        &content_ref,
        verified.bytes().len() as u64,
        content_type,
    )
    .await?;

    Ok(json!({
        "id": target_id.to_string(),
        "kinds": kinds,
        "result": Value::Object(result),
        "status": if refusal.is_some() { "degraded" } else { "complete" },
        "refusal": refusal,
        "receipt_id": extraction_receipt_id.to_string(),
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
        let site = identity::site_id(&khive_types::Namespace::local(), &canonical);
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
        crate::fetch::root_body(
            runtime,
            id,
            khive_storage::AttachmentSubstrate::Entity,
            &content_ref,
            Some(content_type),
            body.len() as u64,
        )
        .await
        .unwrap();
        id
    }

    async fn capture_page(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        id: Uuid,
        body: &[u8],
        link_headers: &[&str],
    ) -> (String, Uuid) {
        let store = crate::blob_store(runtime).unwrap();
        let content_ref = store.put(body.to_vec()).await.unwrap();
        let reference = content_ref.to_string();
        crate::entities::patch(
            runtime,
            token,
            id,
            None,
            json!({ "blob_ref": reference, "content_type": "text/html" }),
        )
        .await
        .unwrap();
        crate::fetch::root_body(
            runtime,
            id,
            khive_storage::AttachmentSubstrate::Entity,
            &content_ref,
            Some("text/html"),
            body.len() as u64,
        )
        .await
        .unwrap();
        let receipt_id = crate::receipt::write_receipt(
            runtime,
            token,
            "web.fetch GET test capture",
            json!({
                "verb": "web.fetch",
                "content_ref": reference,
                "body_entity_id": id.to_string(),
                "headers": { "link": link_headers },
            }),
            vec![id],
        )
        .await
        .unwrap();
        crate::entities::patch(
            runtime,
            token,
            id,
            None,
            json!({ "capture_receipt_id": receipt_id.to_string() }),
        )
        .await
        .unwrap();
        (reference, receipt_id)
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

    // Set-like kind selection must not hide work committed by an earlier pass.
    #[tokio::test]
    async fn duplicate_sitemap_kind_preserves_admitted_count() {
        for repetitions in [1usize, 2] {
            let (runtime, token, _dir) = test_runtime().await;
            let document = seed_page(
                &runtime,
                &token,
                "https://duplicate-kind.example.test/map.xml",
                "application/xml",
                b"<urlset><url><loc>https://duplicate-kind.example.test/entry</loc></url></urlset>",
            )
            .await;
            let kinds = vec!["sitemap"; repetitions];
            let pack = crate::WebPack::new(runtime.clone());
            let reply = pack
                .handle_extract(
                    &token,
                    json!({ "id": document, "kinds": kinds, "link_limit": 1 }),
                )
                .await
                .unwrap();
            let site = identity::site_id(
                &khive_types::Namespace::local(),
                &Url::parse("https://duplicate-kind.example.test/map.xml").unwrap(),
            );
            let neighbors = runtime
                .neighbors(
                    &token,
                    site,
                    khive_storage::Direction::Out,
                    None,
                    Some(vec![EdgeRelation::Contains]),
                )
                .await
                .unwrap();
            assert_eq!(
                neighbors.len(),
                1,
                "the admitted target remains in the graph"
            );
            assert_eq!(
                reply["result"]["sitemap"]["entries"], 1,
                "duplicate kind must not overwrite earlier admitted work with zero"
            );
        }
    }

    #[tokio::test]
    async fn sitemap_and_feed_share_a_bounded_entry_budget_and_report_skips() {
        let (runtime, token, _dir) = test_runtime().await;
        let mut body = String::from("<urlset>");
        for index in 0..5_000 {
            body.push_str(&format!(
                "<url><loc>https://entries.example.test/{index}</loc></url>"
            ));
        }
        body.push_str("<link href=\"https://entries.example.test/feed-a\"/><link href=\"https://entries.example.test/feed-b\"/></urlset>");
        let document = seed_page(
            &runtime,
            &token,
            "https://publisher.example.test/map.xml",
            "application/xml",
            body.as_bytes(),
        )
        .await;

        let reply = run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(document),
                url: None,
                kinds: Some(vec!["sitemap".into(), "feed".into()]),
                link_limit: Some(3),
                namespace: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(reply["result"]["sitemap"]["entries"], 3);
        assert_eq!(reply["result"]["sitemap"]["skipped"], 4_997);
        assert_eq!(reply["result"]["feed"]["entries"], 0);
        assert_eq!(reply["result"]["feed"]["skipped"], 2);

        let site = identity::site_id(
            &khive_types::Namespace::local(),
            &Url::parse("https://publisher.example.test/map.xml").unwrap(),
        );
        let neighbors = runtime
            .neighbors(
                &token,
                site,
                khive_storage::Direction::Out,
                None,
                Some(vec![EdgeRelation::Contains]),
            )
            .await
            .unwrap();
        assert_eq!(
            neighbors.len(),
            3,
            "only admitted entries acquire graph edges"
        );
        let fourth = Url::parse("https://entries.example.test/3").unwrap();
        let fourth_id = identity::document_id(
            identity::site_id(&khive_types::Namespace::local(), &fourth),
            &identity::path_and_query(&fourth),
        );
        assert!(runtime
            .entities(&token)
            .unwrap()
            .get_entity(fourth_id)
            .await
            .unwrap()
            .is_none());
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
        let roots = runtime
            .core()
            .attachments()
            .unwrap()
            .list_attachments(entity.id)
            .await
            .unwrap();
        assert_eq!(
            roots.len(),
            1,
            "repeated extraction retains one content root"
        );
        assert_eq!(roots[0].role, "content");
        assert_eq!(roots[0].content_ref, content_ref);
        assert_eq!(roots[0].media_type.as_deref(), Some("text/plain"));
        assert_eq!(roots[0].size_bytes, Some(bytes.len() as u64));
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

    #[tokio::test]
    async fn two_captures_keep_distinct_excerpts_and_retract_missing_live_links() {
        let (runtime, token, _dir) = test_runtime().await;
        let first = b"<p>Same words</p><a href='/old' rel='next'>Link</a>";
        let second = b"<section>Same words</section><a href='/new' rel='next'>Link</a>";
        let page_id = seed_page(
            &runtime,
            &token,
            "https://capture.example.test/page",
            "text/html",
            first,
        )
        .await;
        let (first_ref, first_capture) = capture_page(&runtime, &token, page_id, first, &[]).await;
        crate::receipt::write_receipt(
            &runtime,
            &token,
            "web.fetch HEAD after test capture",
            json!({ "verb": "web.fetch", "method": "HEAD", "content_ref": null }),
            vec![page_id],
        )
        .await
        .unwrap();
        let params = || ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["text".into(), "links".into()]),
            link_limit: None,
            namespace: None,
        };
        let first_reply = run_extract(&runtime, &token, params()).await.unwrap();
        let first_text =
            Uuid::parse_str(first_reply["result"]["text"]["id"].as_str().unwrap()).unwrap();
        let first_extraction =
            Uuid::parse_str(first_reply["receipt_id"].as_str().unwrap()).unwrap();

        let (second_ref, second_capture) =
            capture_page(&runtime, &token, page_id, second, &[]).await;
        let second_reply = run_extract(&runtime, &token, params()).await.unwrap();
        let second_text =
            Uuid::parse_str(second_reply["result"]["text"]["id"].as_str().unwrap()).unwrap();
        let second_extraction =
            Uuid::parse_str(second_reply["receipt_id"].as_str().unwrap()).unwrap();

        assert_ne!(first_ref, second_ref);
        assert_ne!(
            first_text, second_text,
            "body identity must not be excerpt identity"
        );
        let notes = runtime.notes(&token).unwrap();
        for (id, expected_ref, expected_capture, expected_target) in [
            (first_extraction, &first_ref, first_capture, "/old"),
            (second_extraction, &second_ref, second_capture, "/new"),
        ] {
            let note = notes.get_note(id).await.unwrap().unwrap();
            let properties = note.properties.unwrap();
            let request = &properties["request"];
            assert_eq!(request["source_content_ref"], expected_ref.as_str());
            assert_eq!(request["capture_receipt_id"], expected_capture.to_string());
            assert!(request["links"][0]["url"]
                .as_str()
                .unwrap()
                .ends_with(expected_target));
            let source = runtime
                .core()
                .attachments()
                .unwrap()
                .get_attachment(id, "source")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(source.content_ref.to_string(), expected_ref.as_str());
        }
        let entities = runtime.entities(&token).unwrap();
        let old_text = entities.get_entity(first_text).await.unwrap().unwrap();
        let new_text = entities.get_entity(second_text).await.unwrap().unwrap();
        assert_eq!(
            old_text.properties.as_ref().unwrap()["source_content_ref"],
            first_ref
        );
        assert_eq!(
            new_text.properties.as_ref().unwrap()["source_content_ref"],
            second_ref
        );
        assert_eq!(
            old_text.properties.as_ref().unwrap()["blob_ref"],
            new_text.properties.as_ref().unwrap()["blob_ref"],
            "two different HTML bodies can produce the same excerpt bytes"
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
        assert_eq!(neighbors.len(), 1);
        let current = entities
            .get_entity(neighbors[0].node_id)
            .await
            .unwrap()
            .unwrap();
        assert!(current.name.ends_with("/new"));

        capture_page(&runtime, &token, page_id, first, &[]).await;
        run_extract(&runtime, &token, params()).await.unwrap();
        let restored = runtime
            .neighbors(
                &token,
                page_id,
                khive_storage::Direction::Out,
                None,
                Some(vec![EdgeRelation::LinksTo]),
            )
            .await
            .unwrap();
        assert_eq!(restored.len(), 1);
        let restored_target = entities
            .get_entity(restored[0].node_id)
            .await
            .unwrap()
            .unwrap();
        assert!(restored_target.name.ends_with("/old"));
    }

    #[tokio::test]
    async fn link_rel_context_occurrences_and_header_survive_on_edges_and_receipt() {
        let (runtime, token, _dir) = test_runtime().await;
        let body = br#"<a href="/same" rel="next prev">Continue</a>
            <a href="/same" rel="license">Terms</a>
            <link href="/style.css" rel="stylesheet" title="Main style">"#;
        let page_id = seed_page(
            &runtime,
            &token,
            "https://links.example.test/page",
            "text/html",
            body,
        )
        .await;
        capture_page(
            &runtime,
            &token,
            page_id,
            body,
            &["<https://links.example.test/legal>; rel=license; title=Legal"],
        )
        .await;
        let reply = run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(page_id),
                url: None,
                kinds: Some(vec!["links".into()]),
                link_limit: None,
                namespace: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(reply["result"]["links"]["edges_created"], 3);
        let edges = runtime
            .list_edges(
                &token,
                EdgeListFilter {
                    source_id: Some(page_id),
                    relations: vec![EdgeRelation::LinksTo],
                    ..Default::default()
                },
                100,
                0,
            )
            .await
            .unwrap();
        assert_eq!(edges.len(), 3);
        let same = identity::canonicalize(Url::parse("https://links.example.test/same").unwrap());
        let same_id = identity::document_id(
            identity::site_id(&khive_types::Namespace::local(), &same),
            &identity::path_and_query(&same),
        );
        let same_edge = edges.iter().find(|edge| edge.target_id == same_id).unwrap();
        let metadata = same_edge.metadata.as_ref().unwrap();
        assert_eq!(metadata["occurrence_count"], 2);
        assert_eq!(metadata["occurrences"][0]["rel"], json!(["next", "prev"]));
        assert_eq!(metadata["occurrences"][0]["context"], "Continue");
        assert_eq!(metadata["occurrences"][1]["rel"], json!(["license"]));
        assert_eq!(metadata["occurrences"][1]["context"], "Terms");
        assert!(edges.iter().any(
            |edge| edge.metadata.as_ref().unwrap()["occurrences"][0]["rel"]
                == json!(["stylesheet"])
        ));
        assert!(edges
            .iter()
            .any(|edge| edge.metadata.as_ref().unwrap()["occurrences"][0]["source"] == "header"));

        let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
        let receipt = runtime
            .notes(&token)
            .unwrap()
            .get_note(receipt_id)
            .await
            .unwrap()
            .unwrap();
        let properties = receipt.properties.unwrap();
        let request = &properties["request"];
        assert_eq!(request["links"].as_array().unwrap().len(), 3);
        assert_eq!(request["links_complete"], true);

        let limited = run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(page_id),
                url: None,
                kinds: Some(vec!["links".into()]),
                link_limit: Some(1),
                namespace: None,
            },
        )
        .await
        .unwrap();
        let live = runtime
            .list_edges(
                &token,
                EdgeListFilter {
                    source_id: Some(page_id),
                    relations: vec![EdgeRelation::LinksTo],
                    ..Default::default()
                },
                100,
                0,
            )
            .await
            .unwrap();
        assert_eq!(live.len(), 3, "budget-skipped present links stay live");
        assert!(live.iter().any(|edge| edge.target_id == same_id));
        let limited_receipt = runtime
            .notes(&token)
            .unwrap()
            .get_note(Uuid::parse_str(limited["receipt_id"].as_str().unwrap()).unwrap())
            .await
            .unwrap()
            .unwrap();
        let limited_properties = limited_receipt.properties.unwrap();
        let limited_request = &limited_properties["request"];
        assert_eq!(limited_request["links_complete"], false);
        assert_eq!(limited_request["links"].as_array().unwrap().len(), 3);
        assert_eq!(limited_request["links"][1]["admitted"], false);
        assert!(limited_request["links"][1]["edge_id"].is_string());
    }

    #[tokio::test]
    async fn legacy_links_are_preserved_with_present_collision_and_absence_evidence() {
        let (runtime, token, _dir) = test_runtime().await;
        let first_body = b"<a href='/present'>Present</a><a href='/deleted'>Deleted</a>";
        let page_id = seed_page(
            &runtime,
            &token,
            "https://legacy.example.test/page",
            "text/html",
            first_body,
        )
        .await;
        let present_id = seed_page(
            &runtime,
            &token,
            "https://legacy.example.test/present",
            "text/html",
            b"<p>Target</p>",
        )
        .await;
        let absent_id = seed_page(
            &runtime,
            &token,
            "https://legacy.example.test/absent",
            "text/html",
            b"<p>Target</p>",
        )
        .await;
        let deleted_id = seed_page(
            &runtime,
            &token,
            "https://legacy.example.test/deleted",
            "text/html",
            b"<p>Target</p>",
        )
        .await;
        runtime
            .link(
                &token,
                page_id,
                present_id,
                EdgeRelation::LinksTo,
                0.7,
                Some(json!({ "legacy_label": "kept" })),
            )
            .await
            .unwrap();
        runtime
            .link(&token, page_id, absent_id, EdgeRelation::LinksTo, 1.0, None)
            .await
            .unwrap();
        let deleted_edge = runtime
            .link(
                &token,
                page_id,
                deleted_id,
                EdgeRelation::LinksTo,
                1.0,
                None,
            )
            .await
            .unwrap();
        runtime
            .delete_edge(&token, Uuid::from(deleted_edge.id), false)
            .await
            .unwrap();
        capture_page(&runtime, &token, page_id, first_body, &[]).await;
        let params = || ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["links".into()]),
            link_limit: Some(2),
            namespace: None,
        };
        let first = run_extract(&runtime, &token, params()).await.unwrap();
        assert_eq!(first["result"]["links"]["edges_created"], 0);
        assert_eq!(first["result"]["links"]["admitted_targets"], 2);
        assert_eq!(first["result"]["links"]["ownership_collisions"], 2);
        let filter = EdgeListFilter {
            source_id: Some(page_id),
            relations: vec![EdgeRelation::LinksTo],
            ..Default::default()
        };
        let edges = runtime
            .list_edges(&token, filter.clone(), 100, 0)
            .await
            .unwrap();
        assert_eq!(edges.len(), 2);
        let present_edge = edges
            .iter()
            .find(|edge| edge.target_id == present_id)
            .unwrap();
        assert_eq!(
            present_edge.metadata.as_ref().unwrap(),
            &json!({ "legacy_label": "kept" })
        );
        assert_eq!(present_edge.weight, 0.7);
        let absent_edge = edges
            .iter()
            .find(|edge| edge.target_id == absent_id)
            .unwrap();
        assert!(absent_edge.metadata.is_none());
        let first_receipt = runtime
            .notes(&token)
            .unwrap()
            .get_note(Uuid::parse_str(first["receipt_id"].as_str().unwrap()).unwrap())
            .await
            .unwrap()
            .unwrap();
        let first_properties = first_receipt.properties.unwrap();
        let first_request = &first_properties["request"];
        assert_eq!(first_request["links"][0]["admitted"], true);
        assert_eq!(first_request["links"][0]["ownership_collision"], true);
        assert_eq!(first_request["links"][1]["ownership_collision"], true);
        assert_eq!(first_request["links_complete"], false);
        assert!(first_request["legacy_claimed"]
            .as_array()
            .unwrap()
            .is_empty());
        let unclaimed = first_request["legacy_unclaimed"].as_array().unwrap();
        assert_eq!(unclaimed.len(), 3);
        assert!(unclaimed.iter().any(|edge| {
            edge["target_id"] == present_id.to_string()
                && edge["present_in_extraction"] == true
                && edge["collides_with_admitted_target"] == true
        }));
        assert!(unclaimed.iter().any(|edge| {
            edge["target_id"] == absent_id.to_string() && edge["present_in_extraction"] == false
        }));
        assert!(unclaimed.iter().any(|edge| {
            edge["target_id"] == deleted_id.to_string()
                && edge["live"] == false
                && edge["collides_with_admitted_target"] == true
        }));
        let tombstone = runtime
            .get_edge_by_natural_key_including_deleted(
                &token,
                token.namespace().as_str(),
                page_id,
                deleted_id,
                EdgeRelation::LinksTo,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(tombstone.deleted_at.is_some());

        capture_page(&runtime, &token, page_id, b"<p>No links</p>", &[]).await;
        let second = run_extract(&runtime, &token, params()).await.unwrap();
        let edges = runtime.list_edges(&token, filter, 100, 0).await.unwrap();
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().any(|edge| {
            edge.target_id == present_id
                && edge.metadata.as_ref() == Some(&json!({ "legacy_label": "kept" }))
                && edge.weight == 0.7
        }));
        assert!(edges
            .iter()
            .any(|edge| edge.target_id == absent_id && edge.metadata.is_none()));
        let second_receipt = runtime
            .notes(&token)
            .unwrap()
            .get_note(Uuid::parse_str(second["receipt_id"].as_str().unwrap()).unwrap())
            .await
            .unwrap()
            .unwrap();
        let second_properties = second_receipt.properties.unwrap();
        assert_eq!(
            second_properties["request"]["legacy_unclaimed"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn legacy_default_present_is_claimed_caller_metadata_collides_and_absent_survives() {
        let (runtime, token, _dir) = test_runtime().await;
        let first_body = b"<a href='/default'>Default</a><a href='/caller'>Caller</a>";
        let page_id = seed_page(
            &runtime,
            &token,
            "https://claim.example.test/page",
            "text/html",
            first_body,
        )
        .await;
        let default_id = seed_page(
            &runtime,
            &token,
            "https://claim.example.test/default",
            "text/html",
            b"<p>Default</p>",
        )
        .await;
        let caller_id = seed_page(
            &runtime,
            &token,
            "https://claim.example.test/caller",
            "text/html",
            b"<p>Caller</p>",
        )
        .await;
        let absent_id = seed_page(
            &runtime,
            &token,
            "https://claim.example.test/absent",
            "text/html",
            b"<p>Absent</p>",
        )
        .await;
        let original_default = runtime
            .link(
                &token,
                page_id,
                default_id,
                EdgeRelation::LinksTo,
                1.0,
                None,
            )
            .await
            .unwrap();
        runtime
            .link(
                &token,
                page_id,
                caller_id,
                EdgeRelation::LinksTo,
                1.0,
                Some(json!({ "caller_label": "keep" })),
            )
            .await
            .unwrap();
        runtime
            .link(&token, page_id, absent_id, EdgeRelation::LinksTo, 1.0, None)
            .await
            .unwrap();
        capture_page(&runtime, &token, page_id, first_body, &[]).await;
        let params = || ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["links".into()]),
            link_limit: Some(2),
            namespace: None,
        };
        let first = run_extract(&runtime, &token, params()).await.unwrap();
        assert_eq!(first["result"]["links"]["edges_created"], 1);
        assert_eq!(first["result"]["links"]["admitted_targets"], 2);
        assert_eq!(first["result"]["links"]["ownership_collisions"], 1);
        let filter = EdgeListFilter {
            source_id: Some(page_id),
            relations: vec![EdgeRelation::LinksTo],
            ..Default::default()
        };
        let edges = runtime
            .list_edges(&token, filter.clone(), 100, 0)
            .await
            .unwrap();
        assert_eq!(edges.len(), 3);
        let default = edges
            .iter()
            .find(|edge| edge.target_id == default_id)
            .unwrap();
        assert_eq!(default.id, original_default.id);
        assert_eq!(default.metadata.as_ref().unwrap()["web_extract"], true);
        let caller = edges
            .iter()
            .find(|edge| edge.target_id == caller_id)
            .unwrap();
        assert_eq!(
            caller.metadata.as_ref().unwrap(),
            &json!({ "caller_label": "keep" })
        );
        let absent = edges
            .iter()
            .find(|edge| edge.target_id == absent_id)
            .unwrap();
        assert!(absent.metadata.is_none());
        let receipt = runtime
            .notes(&token)
            .unwrap()
            .get_note(Uuid::parse_str(first["receipt_id"].as_str().unwrap()).unwrap())
            .await
            .unwrap()
            .unwrap();
        let receipt_properties = receipt.properties.unwrap();
        let request = &receipt_properties["request"];
        assert_eq!(request["links_complete"], false);
        let claimed = request["legacy_claimed"].as_array().unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(
            claimed[0]["edge_id"],
            Uuid::from(original_default.id).to_string()
        );
        assert_eq!(claimed[0]["target_id"], default_id.to_string());
        assert_eq!(claimed[0]["admitted"], true);
        let unclaimed = request["legacy_unclaimed"].as_array().unwrap();
        assert_eq!(unclaimed.len(), 2);
        assert!(unclaimed.iter().any(|edge| {
            edge["target_id"] == caller_id.to_string()
                && edge["present_in_extraction"] == true
                && edge["collides_with_admitted_target"] == true
        }));
        assert!(unclaimed.iter().any(|edge| {
            edge["target_id"] == absent_id.to_string() && edge["present_in_extraction"] == false
        }));

        capture_page(&runtime, &token, page_id, b"<p>No links</p>", &[]).await;
        run_extract(&runtime, &token, params()).await.unwrap();
        let edges = runtime.list_edges(&token, filter, 100, 0).await.unwrap();
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|edge| edge.target_id != default_id));
        assert!(edges.iter().any(|edge| {
            edge.target_id == caller_id
                && edge.metadata.as_ref() == Some(&json!({ "caller_label": "keep" }))
        }));
        assert!(edges
            .iter()
            .any(|edge| edge.target_id == absent_id && edge.metadata.is_none()));
    }

    #[tokio::test]
    async fn legacy_default_present_is_claimed_even_when_budget_skips_target() {
        let (runtime, token, _dir) = test_runtime().await;
        let first_body = b"<a href='/target'>Target</a>";
        let page_id = seed_page(
            &runtime,
            &token,
            "https://claim.example.test/limited",
            "text/html",
            first_body,
        )
        .await;
        let target_id = seed_page(
            &runtime,
            &token,
            "https://claim.example.test/target",
            "text/html",
            b"<p>Target</p>",
        )
        .await;
        runtime
            .link(&token, page_id, target_id, EdgeRelation::LinksTo, 1.0, None)
            .await
            .unwrap();
        capture_page(&runtime, &token, page_id, first_body, &[]).await;
        let params = || ExtractParams {
            id: Some(page_id),
            url: None,
            kinds: Some(vec!["links".into()]),
            link_limit: Some(0),
            namespace: None,
        };
        let first = run_extract(&runtime, &token, params()).await.unwrap();
        assert_eq!(first["result"]["links"]["admitted_targets"], 0);
        assert_eq!(first["result"]["links"]["skipped"], 1);
        let receipt = runtime
            .notes(&token)
            .unwrap()
            .get_note(Uuid::parse_str(first["receipt_id"].as_str().unwrap()).unwrap())
            .await
            .unwrap()
            .unwrap();
        let receipt_properties = receipt.properties.unwrap();
        let request = &receipt_properties["request"];
        assert_eq!(
            request["legacy_claimed"][0]["target_id"],
            target_id.to_string()
        );
        assert_eq!(request["legacy_claimed"][0]["admitted"], false);
        let edge = runtime
            .list_edges(
                &token,
                EdgeListFilter {
                    source_id: Some(page_id),
                    target_id: Some(target_id),
                    relations: vec![EdgeRelation::LinksTo],
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(edge.metadata.as_ref().unwrap()["web_extract"], true);
        capture_page(&runtime, &token, page_id, b"<p>No links</p>", &[]).await;
        run_extract(&runtime, &token, params()).await.unwrap();
        assert!(runtime
            .list_edges(
                &token,
                EdgeListFilter {
                    source_id: Some(page_id),
                    target_id: Some(target_id),
                    relations: vec![EdgeRelation::LinksTo],
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn legacy_nondefault_weight_present_remains_unclaimed() {
        let (runtime, token, _dir) = test_runtime().await;
        let body = b"<a href='/weighted-target'>Target</a>";
        let page_id = seed_page(
            &runtime,
            &token,
            "https://claim.example.test/weighted",
            "text/html",
            body,
        )
        .await;
        let target_id = seed_page(
            &runtime,
            &token,
            "https://claim.example.test/weighted-target",
            "text/html",
            b"<p>Target</p>",
        )
        .await;
        runtime
            .link(&token, page_id, target_id, EdgeRelation::LinksTo, 0.7, None)
            .await
            .unwrap();
        capture_page(&runtime, &token, page_id, body, &[]).await;
        let reply = run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(page_id),
                url: None,
                kinds: Some(vec!["links".into()]),
                link_limit: Some(1),
                namespace: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(reply["result"]["links"]["ownership_collisions"], 1);
        let edges = runtime
            .list_edges(
                &token,
                EdgeListFilter {
                    source_id: Some(page_id),
                    relations: vec![EdgeRelation::LinksTo],
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].weight, 0.7);
        assert!(edges[0].metadata.is_none());
        capture_page(&runtime, &token, page_id, b"<p>No links</p>", &[]).await;
        run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(page_id),
                url: None,
                kinds: Some(vec!["links".into()]),
                link_limit: Some(1),
                namespace: None,
            },
        )
        .await
        .unwrap();
        let edges = runtime
            .list_edges(
                &token,
                EdgeListFilter {
                    source_id: Some(page_id),
                    relations: vec![EdgeRelation::LinksTo],
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].weight, 0.7);
        assert!(edges[0].metadata.is_none());
    }

    #[tokio::test]
    async fn capture_selection_requires_the_body_owner_even_when_digest_and_annotation_match() {
        let (runtime, token, _dir) = test_runtime().await;
        let body = b"<p>Shared bytes</p>";
        let page_id = seed_page(
            &runtime,
            &token,
            "https://owner.example.test/page",
            "text/html",
            body,
        )
        .await;
        let (reference, source_capture) = capture_page(&runtime, &token, page_id, body, &[]).await;
        let other_id = seed_page(
            &runtime,
            &token,
            "https://owner.example.test/other",
            "text/html",
            body,
        )
        .await;
        let other_capture = crate::receipt::write_receipt(
            &runtime,
            &token,
            "redirect participant with identical body digest",
            json!({
                "verb": "web.fetch",
                "content_ref": reference,
                "body_entity_id": other_id.to_string(),
                "headers": { "link": ["<https://owner.example.test/wrong>; rel=next"] },
            }),
            vec![page_id, other_id],
        )
        .await
        .unwrap();
        let page = runtime
            .entities(&token)
            .unwrap()
            .get_entity(page_id)
            .await
            .unwrap()
            .unwrap();
        let selected = crate::receipt::capture_for_body(&runtime, &token, &page, &reference)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(selected.0, source_capture);

        // The pointer is only a hint. If it too names the wrong owner, the
        // capture is unknown rather than misattributed to this document.
        crate::entities::patch(
            &runtime,
            &token,
            page_id,
            None,
            json!({ "capture_receipt_id": other_capture.to_string() }),
        )
        .await
        .unwrap();
        let page = runtime
            .entities(&token)
            .unwrap()
            .get_entity(page_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            crate::receipt::capture_for_body(&runtime, &token, &page, &reference)
                .await
                .unwrap()
                .is_none()
        );
        let reply = run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(page_id),
                url: None,
                kinds: Some(vec!["links".into()]),
                link_limit: Some(10),
                namespace: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(reply["result"]["links"]["edges_created"], 0);
        let receipt = runtime
            .notes(&token)
            .unwrap()
            .get_note(Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(receipt.properties.unwrap()["request"]["capture_receipt_id"].is_null());
    }

    #[tokio::test]
    async fn legacy_unmarked_capture_extracts_without_receipt_or_header_links() {
        let (runtime, token, _dir) = test_runtime().await;
        let body = b"<p>Legacy body without HTML links</p>";
        let page_id = seed_page(
            &runtime,
            &token,
            "https://legacy-capture.example.test/page",
            "text/html",
            body,
        )
        .await;
        let page = runtime
            .entities(&token)
            .unwrap()
            .get_entity(page_id)
            .await
            .unwrap()
            .unwrap();
        let reference = page.properties.as_ref().unwrap()["blob_ref"]
            .as_str()
            .unwrap()
            .to_string();
        let legacy = runtime
            .create_note(
                &token,
                "observation",
                None,
                "pre-upgrade web receipt",
                None,
                Some(json!({
                    "tags": [crate::receipt::RECEIPT_TAG],
                    "request": {
                        "verb": "web.fetch",
                        "content_ref": reference.clone(),
                        "body_entity_id": page_id.to_string(),
                        "headers": {"link": ["<https://legacy-capture.example.test/header>; rel=next"]},
                    },
                })),
                vec![page_id],
            )
            .await
            .unwrap();
        crate::entities::patch(
            &runtime,
            &token,
            page_id,
            None,
            json!({ "capture_receipt_id": legacy.id.to_string() }),
        )
        .await
        .unwrap();
        let page = runtime
            .entities(&token)
            .unwrap()
            .get_entity(page_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            crate::receipt::capture_for_body(&runtime, &token, &page, &reference)
                .await
                .unwrap()
                .is_none()
        );

        let reply = run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(page_id),
                url: None,
                kinds: Some(vec!["links".into()]),
                link_limit: Some(10),
                namespace: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(reply["result"]["links"]["edges_created"], 0);
        let extraction_note = runtime
            .notes(&token)
            .unwrap()
            .get_note(Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(extraction_note.properties.unwrap()["request"]["capture_receipt_id"].is_null());
        assert!(runtime
            .notes(&token)
            .unwrap()
            .get_note(legacy.id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn extraction_uses_genuine_capture_behind_newer_tagged_decoy() {
        let (runtime, token, _dir) = test_runtime().await;
        let body = b"<p>Captured body</p>";
        let page_id = seed_page(
            &runtime,
            &token,
            "https://owner.example.test/tagged-decoy",
            "text/html",
            body,
        )
        .await;
        let (_reference, genuine) = capture_page(&runtime, &token, page_id, body, &[]).await;
        let genuine_note = runtime
            .notes(&token)
            .unwrap()
            .get_note(genuine)
            .await
            .unwrap()
            .unwrap();
        let decoy = runtime
            .create_note(
                &token,
                "observation",
                None,
                "caller-written tagged decoy",
                None,
                Some(json!({"tags": [crate::receipt::RECEIPT_TAG]})),
                vec![page_id],
            )
            .await
            .unwrap();
        let mut newer_decoy = decoy.clone();
        newer_decoy.created_at = genuine_note.created_at + 1;
        newer_decoy.updated_at = newer_decoy.created_at;
        runtime
            .backend()
            .notes()
            .unwrap()
            .upsert_note(newer_decoy)
            .await
            .unwrap();
        assert_eq!(
            runtime
                .latest_annotating_note(&token, page_id, "observation", crate::receipt::RECEIPT_TAG)
                .await
                .unwrap(),
            Some(decoy.id)
        );
        crate::entities::patch(
            &runtime,
            &token,
            page_id,
            None,
            json!({ "capture_receipt_id": null }),
        )
        .await
        .unwrap();

        let reply = run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(page_id),
                url: None,
                kinds: Some(vec!["text".into()]),
                link_limit: None,
                namespace: None,
            },
        )
        .await
        .unwrap();
        let extraction_note = runtime
            .notes(&token)
            .unwrap()
            .get_note(Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            extraction_note.properties.unwrap()["request"]["capture_receipt_id"],
            genuine.to_string()
        );
    }

    #[tokio::test]
    async fn extraction_refuses_property_and_content_attachment_mismatch_before_writes() {
        let (runtime, token, _dir) = test_runtime().await;
        let page_id = seed_page(
            &runtime,
            &token,
            "https://owner.example.test/mismatch",
            "text/html",
            b"<a href='/old'>Old</a>",
        )
        .await;
        let store = crate::blob_store(&runtime).unwrap();
        let second = store.put(b"<a href='/new'>New</a>".to_vec()).await.unwrap();
        crate::entities::patch(
            &runtime,
            &token,
            page_id,
            None,
            json!({ "blob_ref": second.to_string() }),
        )
        .await
        .unwrap();
        assert!(!crate::receipt::bind_capture_receipt(
            &runtime,
            &token,
            page_id,
            second.as_ref(),
            Uuid::new_v4(),
        )
        .await
        .unwrap());
        let page = runtime
            .entities(&token)
            .unwrap()
            .get_entity(page_id)
            .await
            .unwrap()
            .unwrap();
        assert!(page
            .properties
            .as_ref()
            .and_then(|properties| properties.get("capture_receipt_id"))
            .is_none());
        let error = run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(page_id),
                url: None,
                kinds: Some(vec!["links".into(), "text".into()]),
                link_limit: None,
                namespace: None,
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, RuntimeError::InvalidInput(ref reason) if reason.starts_with("capture_changed:"))
        );
        assert!(runtime
            .latest_annotating_note(
                &token,
                page_id,
                "observation",
                crate::receipt::EXTRACTION_RECEIPT_TAG
            )
            .await
            .unwrap()
            .is_none());
        assert!(runtime
            .list_edges(
                &token,
                EdgeListFilter {
                    source_id: Some(page_id),
                    relations: vec![EdgeRelation::LinksTo],
                    ..Default::default()
                },
                100,
                0,
            )
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn occurrence_limit_writes_degraded_receipt_without_link_mutation() {
        let (runtime, token, _dir) = test_runtime().await;
        let body = "<a href='/same'>Same</a>".repeat(MAX_LINK_OCCURRENCES + 1);
        let page_id = seed_page(
            &runtime,
            &token,
            "https://limit.example.test/page",
            "text/html",
            body.as_bytes(),
        )
        .await;
        let marked_id = seed_page(
            &runtime,
            &token,
            "https://limit.example.test/marked",
            "text/html",
            b"<p>Target</p>",
        )
        .await;
        let unmarked_id = seed_page(
            &runtime,
            &token,
            "https://limit.example.test/unmarked",
            "text/html",
            b"<p>Target</p>",
        )
        .await;
        runtime
            .link(
                &token,
                page_id,
                marked_id,
                EdgeRelation::LinksTo,
                1.0,
                Some(json!({ "web_extract": true })),
            )
            .await
            .unwrap();
        runtime
            .link(
                &token,
                page_id,
                unmarked_id,
                EdgeRelation::LinksTo,
                1.0,
                None,
            )
            .await
            .unwrap();
        let (source_ref, _) = capture_page(&runtime, &token, page_id, body.as_bytes(), &[]).await;
        let reply = run_extract(
            &runtime,
            &token,
            ExtractParams {
                id: Some(page_id),
                url: None,
                kinds: Some(vec!["links".into(), "text".into()]),
                link_limit: Some(1),
                namespace: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(reply["status"], "degraded");
        assert_eq!(reply["result"]["links"]["refused"], true);
        assert!(reply["result"]["text"]["id"].is_string());
        assert_eq!(reply["refusal"]["code"], "too_many_link_occurrences");
        let edges = runtime
            .list_edges(
                &token,
                EdgeListFilter {
                    source_id: Some(page_id),
                    relations: vec![EdgeRelation::LinksTo],
                    ..Default::default()
                },
                100,
                0,
            )
            .await
            .unwrap();
        assert_eq!(
            edges.len(),
            2,
            "the refused links kind does not reconcile edges"
        );
        assert_eq!(
            edges
                .iter()
                .find(|edge| edge.target_id == marked_id)
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()["web_extract"],
            true
        );
        assert!(edges
            .iter()
            .find(|edge| edge.target_id == unmarked_id)
            .unwrap()
            .metadata
            .is_none());
        let receipt_id = Uuid::parse_str(reply["receipt_id"].as_str().unwrap()).unwrap();
        let note = runtime
            .notes(&token)
            .unwrap()
            .get_note(receipt_id)
            .await
            .unwrap()
            .unwrap();
        let properties = note.properties.unwrap();
        let request = &properties["request"];
        assert_eq!(request["status"], "degraded");
        assert_eq!(request["refusal"]["code"], "too_many_link_occurrences");
        assert_eq!(request["links_complete"], false);
        assert!(request["legacy_claimed"].is_null());
        assert!(request["legacy_unclaimed"].is_null());
        let source = runtime
            .core()
            .attachments()
            .unwrap()
            .get_attachment(receipt_id, "source")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source.content_ref.to_string(), source_ref);
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
            blake3::hash(html.as_bytes()).to_hex().as_ref(),
            None,
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
        let site = identity::site_id(&khive_types::Namespace::local(), &canonical);
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
