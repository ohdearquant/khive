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

    let store = runtime.require_blob_store()?;
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
    let hydrator = runtime.require_blob_hydrator()?;
    verify_source_body(runtime, token, target_id, &source_content_ref).await?;
    let blob_store = runtime.require_blob_store()?;
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
mod tests;
