//! Deterministic inbound screening (ADR-200 sections 8.3 and 8.4).
//!
//! Screening runs at ingest with no network and no model. It is a pure
//! function over a small input struct, plus a constructor from a parsed
//! `mail_parser::Message`. The rules live in a versioned JSON data file
//! (`rules/screening.json`), compiled into the binary with `include_str!`
//! and parsed once at first use. A malformed rules file is a load error,
//! never a panic on the ingest path.

use std::sync::OnceLock;

use mail_parser::{Message, MimeHeaders};

/// The shipped rules file, compiled into the binary.
pub const SCREENING_RULES_JSON: &str = include_str!("../rules/screening.json");

/// A screening rule family (ADR-200 section 8.3, plus the opt-out family of
/// section 8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    /// Unicode format characters outside emoji sequences, or HTML text hidden
    /// from a sighted reader.
    HiddenText,
    /// Text addressed to an automated reader.
    AgentAddressed,
    /// A request to run, install or execute something, or to supply a credential.
    ActionRequest,
    /// An attachment whose declared or sniffed type is executable or a script.
    ExecutableAttachment,
    /// No text/plain part; the message is delivered with text extracted from HTML.
    HtmlOnly,
    /// Explicit opt-out phrasing; never changes the delivery outcome.
    OptOut,
}

impl Family {
    /// The wire name used in the rules file and in screening labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Family::HiddenText => "hidden_text",
            Family::AgentAddressed => "agent_addressed",
            Family::ActionRequest => "action_request",
            Family::ExecutableAttachment => "executable_attachment",
            Family::HtmlOnly => "html_only",
            Family::OptOut => "opt_out",
        }
    }

    /// The three families whose presence withholds delivery (section 8.3).
    fn withholds(self) -> bool {
        matches!(
            self,
            Family::HiddenText | Family::AgentAddressed | Family::ActionRequest
        )
    }
}

/// One screening rule.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Rule {
    /// Stable rule identifier, reported as a screening label.
    pub id: String,
    /// The family this rule belongs to.
    pub family: Family,
    /// Human-readable description of what the rule matches.
    pub description: String,
    /// Match terms. A bracketed token selects a structural matcher; every
    /// other term is a case-insensitive phrase.
    #[serde(rename = "match")]
    pub terms: Vec<String>,
}

/// The parsed rules file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RuleSet {
    /// Rules-file format version.
    pub version: u32,
    /// The rules, in file order.
    pub rules: Vec<Rule>,
}

/// Failure to load the rules file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScreeningError {
    /// The compiled-in rules file is not valid.
    #[error("screening rules are invalid: {0}")]
    RulesInvalid(String),
}

static RULES: OnceLock<Result<RuleSet, ScreeningError>> = OnceLock::new();

/// Parse the compiled-in rules file once and return the shared rule set.
pub fn rules() -> Result<&'static RuleSet, ScreeningError> {
    RULES
        .get_or_init(|| parse_rules(SCREENING_RULES_JSON))
        .as_ref()
        .map_err(|e| e.clone())
}

/// The rules-file format version this build reads.
const RULES_VERSION: u32 = 1;

fn parse_rules(json: &str) -> Result<RuleSet, ScreeningError> {
    let rule_set: RuleSet =
        serde_json::from_str(json).map_err(|e| ScreeningError::RulesInvalid(e.to_string()))?;
    if rule_set.version != RULES_VERSION {
        return Err(ScreeningError::RulesInvalid(format!(
            "rules-file version {} is not supported; this build reads version {RULES_VERSION}",
            rule_set.version
        )));
    }
    Ok(rule_set)
}

/// One attachment, reduced to what screening needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachmentInput {
    /// The declared content type, as `type/subtype`.
    pub declared_content_type: Option<String>,
    /// The declared file name, if any.
    pub file_name: Option<String>,
    /// The leading bytes of the decoded attachment body, for sniffing.
    pub first_bytes: Vec<u8>,
}

/// The small input struct screening operates over.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScreeningInput {
    /// The message subject.
    pub subject: String,
    /// The decoded text/plain parts.
    pub plain_parts: Vec<String>,
    /// The decoded text/html parts.
    pub html_parts: Vec<String>,
    /// The attachments.
    pub attachments: Vec<AttachmentInput>,
}

impl ScreeningInput {
    /// Build a screening input from a parsed message.
    pub fn from_message(message: &Message<'_>) -> ScreeningInput {
        let subject = message.subject().unwrap_or_default().to_string();

        let plain_parts: Vec<String> = (0..message.text_body_count())
            .filter_map(|pos| message.text_part(pos))
            .filter(|part| !part.is_text_html())
            .filter_map(|part| part.text_contents().map(str::to_string))
            .filter(|text| !text.trim().is_empty())
            .collect();

        let html_parts: Vec<String> = (0..message.html_body_count())
            .filter_map(|pos| message.html_part(pos))
            .filter(|part| part.is_text_html())
            .filter_map(|part| part.text_contents().map(str::to_string))
            .collect();

        let attachments: Vec<AttachmentInput> = (0..message.attachment_count())
            .filter_map(|pos| message.attachment(pos))
            .map(|part| AttachmentInput {
                declared_content_type: part.content_type().map(|ct| match ct.subtype() {
                    Some(subtype) => format!("{}/{}", ct.ctype(), subtype),
                    None => ct.ctype().to_string(),
                }),
                file_name: part.attachment_name().map(str::to_string),
                first_bytes: part.contents().iter().take(8).copied().collect(),
            })
            .collect();

        ScreeningInput {
            subject,
            plain_parts,
            html_parts,
            attachments,
        }
    }
}

/// The disposition screening assigns to a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Deliver, marked. No withholding family matched, or only `html_only`.
    DeliverMarked,
    /// Deliver with the body replaced by a placeholder naming the labels.
    DeliverWithheld,
    /// Quarantine with reason `screened`.
    QuarantineScreened,
}

impl Disposition {
    /// The quarantine reason, when the disposition is quarantine.
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Disposition::QuarantineScreened => Some("screened"),
            _ => None,
        }
    }
}

/// The screening result for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreeningOutcome {
    /// The ids of every rule that matched, in rules-file order.
    pub rule_ids: Vec<String>,
    /// The distinct families that matched, in first-match order.
    pub families: Vec<Family>,
    /// The disposition per ADR-200 section 8.3.
    pub disposition: Disposition,
    /// Whether the opt-out family matched (section 8.4).
    pub opt_out: bool,
}

impl ScreeningOutcome {
    /// Whether any withholding family matched.
    pub fn withheld(&self) -> bool {
        self.disposition == Disposition::DeliverWithheld
    }
}

/// Screen one message. Pure: no I/O, no global state beyond the parsed rules.
///
/// Returns `ScreeningError` only when the compiled-in rules file is invalid.
pub fn screen(input: &ScreeningInput) -> Result<ScreeningOutcome, ScreeningError> {
    let rule_set = rules()?;

    let plain_text = input.plain_parts.join("\n");
    let plain_norm = normalize(&plain_text);
    let corpus = normalize(&corpus_raw(input));

    let mut rule_ids: Vec<String> = Vec::new();
    let mut families: Vec<Family> = Vec::new();

    for rule in &rule_set.rules {
        if rule.family == Family::OptOut {
            continue;
        }
        if rule_matches(rule, &corpus, &plain_norm, input) {
            rule_ids.push(rule.id.clone());
            if !families.contains(&rule.family) {
                families.push(rule.family);
            }
        }
    }

    let opt_out = opt_out_matches(&rule_set.rules, input);
    if opt_out {
        if let Some(rule) = rule_set.rules.iter().find(|r| r.family == Family::OptOut) {
            rule_ids.push(rule.id.clone());
        }
        if !families.contains(&Family::OptOut) {
            families.push(Family::OptOut);
        }
    }

    let disposition = disposition_for(&families);

    Ok(ScreeningOutcome {
        rule_ids,
        families,
        disposition,
        opt_out,
    })
}

/// Screen parsed MIME, using the existing HTML text conversion only for opt-out detection.
pub fn screen_message(message: &Message<'_>) -> Result<ScreeningOutcome, ScreeningError> {
    let input = ScreeningInput::from_message(message);
    let mut outcome = screen(&input)?;
    if !outcome.opt_out && input.plain_parts.is_empty() {
        let extracted = (0..message.text_body_count())
            .filter_map(|pos| message.body_text(pos))
            .map(|text| text.into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(first_line) = extracted
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
        {
            let first_line = normalize(first_line);
            if let Some(rule) = rules()?
                .rules
                .iter()
                .find(|rule| rule.family == Family::OptOut)
            {
                if rule.terms.iter().any(|term| {
                    let needle = normalize(term);
                    !needle.is_empty() && first_line.contains(&needle)
                }) {
                    // HTML quotation false positives intentionally run toward suppression.
                    outcome.opt_out = true;
                    outcome.rule_ids.push(rule.id.clone());
                    outcome.families.push(Family::OptOut);
                }
            }
        }
    }
    Ok(outcome)
}

fn disposition_for(families: &[Family]) -> Disposition {
    if families.contains(&Family::ExecutableAttachment) {
        return Disposition::QuarantineScreened;
    }
    let withholding = families.iter().filter(|f| f.withholds()).count();
    if withholding >= 2 {
        Disposition::QuarantineScreened
    } else if withholding == 1 {
        Disposition::DeliverWithheld
    } else {
        Disposition::DeliverMarked
    }
}

fn rule_matches(rule: &Rule, corpus: &str, plain_norm: &str, input: &ScreeningInput) -> bool {
    for term in &rule.terms {
        match term.as_str() {
            "<cf>" => {
                if has_format_char(&corpus_raw(input)) {
                    return true;
                }
            }
            "<attachment>" => {
                if input.attachments.iter().any(is_executable_attachment) {
                    return true;
                }
            }
            "<no-text-part>" => {
                let no_plain = input.plain_parts.iter().all(|part| part.trim().is_empty());
                if no_plain && !input.html_parts.is_empty() {
                    return true;
                }
            }
            token if token.starts_with("<html-hidden:") => {
                let prop = token
                    .trim_start_matches("<html-hidden:")
                    .trim_end_matches('>');
                if html_hidden_matches(input, prop, plain_norm) {
                    return true;
                }
            }
            phrase => {
                let needle = normalize(phrase);
                if !needle.is_empty() && corpus.contains(&needle) {
                    return true;
                }
            }
        }
    }
    false
}

fn corpus_raw(input: &ScreeningInput) -> String {
    let mut parts = Vec::with_capacity(2 + input.plain_parts.len() + input.html_parts.len());
    parts.push(input.subject.as_str());
    parts.extend(input.plain_parts.iter().map(String::as_str));
    parts.extend(input.html_parts.iter().map(String::as_str));
    parts.join("\n")
}

fn opt_out_matches(rules: &[Rule], input: &ScreeningInput) -> bool {
    let Some(rule) = rules.iter().find(|r| r.family == Family::OptOut) else {
        return false;
    };
    let subject = normalize(&input.subject);
    let first_line = input
        .plain_parts
        .iter()
        .flat_map(|part| part.lines())
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('>'))
        .map(normalize)
        .unwrap_or_default();

    rule.terms.iter().any(|term| {
        let needle = normalize(term);
        !needle.is_empty() && (subject.contains(&needle) || first_line.contains(&needle))
    })
}

/// Case-fold and collapse ASCII whitespace runs to a single space.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        for lc in c.to_lowercase() {
            out.push(lc);
        }
    }
    out
}

/// `true` for a Unicode format character (general category Cf), Unicode 16.0.0.
///
/// The ranges are the Cf set of the Unicode Character Database at version
/// 16.0.0. They are written out here rather than taken from a dependency so
/// that screening adds no crate to the build; the set is stable and small.
fn is_format_char(c: char) -> bool {
    matches!(
        c as u32,
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

/// `true` for a code point that can carry an emoji presentation.
///
/// Conservative on purpose: a broader emoji set means fewer Cf characters are
/// excused, so the safe direction is a narrow predicate.
fn is_emoji_code_point(c: char) -> bool {
    matches!(c as u32, 0x2600..=0x27BF | 0x1F000..=0x1FAFF)
}

/// U+1F3F4 WAVING BLACK FLAG, the base of every RGI emoji tag sequence.
const TAG_BASE: char = '\u{1F3F4}';

/// U+E007F CANCEL TAG, the end of an emoji tag sequence.
const CANCEL_TAG: char = '\u{E007F}';

/// The tag specs of the RGI emoji tag sequences in Unicode 16.0.0: the flags
/// of England, Scotland and Wales.
const RGI_TAG_SPECS: [&str; 3] = ["gbeng", "gbsct", "gbwls"];

/// `true` when `text` carries a Cf character outside an emoji sequence.
///
/// U+200D (zero-width joiner) is excused when the preceding code point,
/// skipping the variation selectors U+FE0E and U+FE0F, is an emoji code
/// point. Tag characters U+E0020..=U+E007F spell ASCII text, so they are
/// excused only inside one of the RGI emoji tag sequences (U+1F3F4, a tag
/// spec from [`RGI_TAG_SPECS`], U+E007F); any other tag character counts.
fn has_format_char(text: &str) -> bool {
    let mut prev: Option<char> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == TAG_BASE {
            let rest = chars.as_str();
            if let Some(len) = rgi_tag_sequence_len(rest) {
                chars = rest[len..].chars();
                prev = Some(TAG_BASE);
                continue;
            }
        }
        if is_format_char(c) {
            let joins_emoji = c == '\u{200D}' && prev.is_some_and(is_emoji_code_point);
            if !joins_emoji {
                return true;
            }
        }
        if !matches!(c, '\u{FE0E}' | '\u{FE0F}') {
            prev = Some(c);
        }
    }
    false
}

/// Byte length of the tag spec and cancel tag at the start of `rest` when
/// they complete an RGI emoji tag sequence after U+1F3F4, else `None`.
fn rgi_tag_sequence_len(rest: &str) -> Option<usize> {
    RGI_TAG_SPECS.iter().find_map(|spec| {
        let mut tags = rest.chars();
        for ascii in spec.chars() {
            let tag = char::from_u32(0xE0000 + u32::from(ascii))?;
            if tags.next()? != tag {
                return None;
            }
        }
        if tags.next()? != CANCEL_TAG {
            return None;
        }
        Some(rest.len() - tags.as_str().len())
    })
}

/// `true` when a hidden HTML segment carries words the plain-text part does not.
fn html_hidden_matches(input: &ScreeningInput, prop: &str, plain_norm: &str) -> bool {
    for html in &input.html_parts {
        for segment in hidden_segments(html, prop) {
            let seg_norm = normalize(&segment);
            if seg_norm.is_empty() {
                continue;
            }
            let carried = seg_norm
                .split(' ')
                .all(|word| plain_norm.split(' ').any(|w| w == word));
            if !carried {
                return true;
            }
        }
    }
    false
}

/// Extract the text of each element whose style hides it by `prop`.
fn hidden_segments(html: &str, prop: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(open_rel) = html[cursor..].find('<') {
        let open = cursor + open_rel;
        let Some(close_rel) = html[open..].find('>') else {
            break;
        };
        let close = open + close_rel;
        let tag = &html[open + 1..close];
        if tag_hidden_property(tag).as_deref() == Some(prop) {
            let text_start = close + 1;
            let text_end = html[text_start..]
                .find('<')
                .map(|p| text_start + p)
                .unwrap_or(html.len());
            let segment = html[text_start..text_end].trim();
            if !segment.is_empty() {
                out.push(segment.to_string());
            }
        }
        cursor = close + 1;
    }
    out
}

/// The hidden property a tag declares, if any.
fn tag_hidden_property(tag: &str) -> Option<String> {
    let style = style_value(tag)?;
    if style_decl(style, "display").is_some_and(|v| v == "none") {
        return Some("display".to_string());
    }
    if style_decl(style, "visibility").is_some_and(|v| v == "hidden" || v == "collapse") {
        return Some("visibility".to_string());
    }
    if style_decl(style, "font-size").is_some_and(|v| zero_length(&v)) {
        return Some("font-size".to_string());
    }
    let fg = style_decl(style, "color");
    let bg = style_decl(style, "background-color").or_else(|| style_decl(style, "background"));
    if let (Some(fg), Some(bg)) = (fg, bg) {
        if !fg.is_empty() && fg == bg {
            return Some("color".to_string());
        }
    }
    None
}

/// Return the value of a `style` attribute, quoted or not.
fn style_value(tag: &str) -> Option<&str> {
    let lower = tag.to_ascii_lowercase();
    let bytes = tag.as_bytes();
    let mut search = 0;
    while let Some(rel) = lower[search..].find("style") {
        let at = search + rel;
        let before_ok = at == 0 || bytes[at - 1].is_ascii_whitespace();
        let mut j = at + "style".len();
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if before_ok && j < bytes.len() && bytes[j] == b'=' {
            let mut k = j + 1;
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < bytes.len() && (bytes[k] == b'"' || bytes[k] == b'\'') {
                let quote = bytes[k];
                let start = k + 1;
                if let Some(end_rel) = tag[start..].find(quote as char) {
                    return Some(&tag[start..start + end_rel]);
                }
                return None;
            }
            let start = k;
            let mut e = start;
            while e < bytes.len() && !bytes[e].is_ascii_whitespace() {
                e += 1;
            }
            return Some(&tag[start..e]);
        }
        search = at + "style".len();
    }
    None
}

/// The lowercased value of a CSS declaration inside a style attribute.
fn style_decl(style: &str, prop: &str) -> Option<String> {
    for decl in style.split(';') {
        if let Some((key, value)) = decl.split_once(':') {
            if key.trim().eq_ignore_ascii_case(prop) {
                return Some(value.trim().to_ascii_lowercase());
            }
        }
    }
    None
}

/// `true` when a CSS length value is zero, with or without a unit.
fn zero_length(value: &str) -> bool {
    let numeric: String = value
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    !numeric.is_empty() && matches!(numeric.parse::<f64>(), Ok(v) if v == 0.0)
}

const EXECUTABLE_CONTENT_TYPES: &[&str] = &[
    "application/x-executable",
    "application/x-msdownload",
    "application/x-dosexec",
    "application/x-msdos-program",
    "application/vnd.microsoft.portable-executable",
    "application/x-sh",
    "application/x-shellscript",
    "application/x-csh",
    "application/x-bat",
    "text/x-shellscript",
    "text/x-sh",
    "application/x-mach-binary",
];

const EXECUTABLE_EXTENSIONS: &[&str] = &[
    "exe", "dll", "com", "bat", "cmd", "msi", "scr", "sh", "bash", "zsh", "ps1", "psm1", "vbs",
    "wsf", "jar", "apk", "app", "bin", "run", "py",
];

const EXECUTABLE_MAGICS: &[&[u8]] = &[
    b"MZ",
    b"\x7fELF",
    b"#!",
    b"\xca\xfe\xba\xbe",
    b"\xfe\xed\xfa\xce",
    b"\xfe\xed\xfa\xcf",
];

fn is_executable_attachment(attachment: &AttachmentInput) -> bool {
    if let Some(declared) = &attachment.declared_content_type {
        let declared = declared.trim().to_ascii_lowercase();
        if EXECUTABLE_CONTENT_TYPES.iter().any(|t| *t == declared) {
            return true;
        }
    }
    if let Some(name) = &attachment.file_name {
        if let Some(ext) = name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
        {
            if EXECUTABLE_EXTENSIONS.iter().any(|e| *e == ext) {
                return true;
            }
        }
    }
    let bytes: &[u8] = &attachment.first_bytes;
    EXECUTABLE_MAGICS
        .iter()
        .any(|magic| bytes.starts_with(magic))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_rules_file_parses() {
        let parsed = rules().expect("the shipped rules file must parse");
        assert_eq!(parsed.version, 1);
        assert!(!parsed.rules.is_empty());
        for family in [
            Family::HiddenText,
            Family::AgentAddressed,
            Family::ActionRequest,
            Family::ExecutableAttachment,
            Family::HtmlOnly,
            Family::OptOut,
        ] {
            assert!(
                parsed.rules.iter().any(|r| r.family == family),
                "no rule for family {}",
                family.as_str()
            );
        }
    }

    #[test]
    fn malformed_rules_file_is_a_typed_error() {
        let err = parse_rules("{ not json").unwrap_err();
        assert!(matches!(err, ScreeningError::RulesInvalid(_)));
    }

    #[test]
    fn unsupported_rules_version_is_a_typed_error() {
        for version in [0, 2] {
            let json = format!(r#"{{"version": {version}, "rules": []}}"#);
            let err = parse_rules(&json).unwrap_err();
            let ScreeningError::RulesInvalid(message) = err;
            assert!(
                message.contains(&format!("version {version} is not supported")),
                "unexpected message for version {version}: {message}"
            );
        }
        assert!(parse_rules(r#"{"version": 1, "rules": []}"#).is_ok());
    }

    /// `U+1F3F4` followed by `spec` spelled in tag characters and, when
    /// `cancel` is set, U+E007F.
    fn tag_sequence(spec: &str, cancel: bool) -> String {
        let mut out = String::from(TAG_BASE);
        for ascii in spec.chars() {
            out.push(char::from_u32(0xE0000 + u32::from(ascii)).unwrap());
        }
        if cancel {
            out.push(CANCEL_TAG);
        }
        out
    }

    #[test]
    fn rgi_flag_tag_sequences_are_not_hidden_text() {
        for spec in RGI_TAG_SPECS {
            let flag = tag_sequence(spec, true);
            assert!(!has_format_char(&flag), "{spec} flag alone");
            assert!(
                !has_format_char(&format!("go {flag}{flag} team")),
                "{spec} flag in text"
            );
        }
    }

    #[test]
    fn tag_characters_outside_an_rgi_sequence_are_hidden_text() {
        // A tag run after the flag base that is not an RGI spec.
        assert!(has_format_char(&tag_sequence("ignore", true)));
        // An RGI spec without its cancel tag.
        assert!(has_format_char(&tag_sequence("gbsct", false)));
        // An RGI flag followed by a further tag run.
        let flag = tag_sequence("gbsct", true);
        assert!(has_format_char(&format!("{flag}\u{E0068}\u{E0069}")));
        // Tag characters after an emoji that is not the flag base.
        assert!(has_format_char("\u{1F600}\u{E0068}\u{E0069}\u{E007F}"));
        // Tag characters with no base at all.
        assert!(has_format_char("note \u{E0068}\u{E0069}"));
    }

    #[test]
    fn zwj_after_a_variation_selector_is_not_hidden_text() {
        // Rainbow flag: U+1F3F3 U+FE0F U+200D U+1F308.
        assert!(!has_format_char("\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}"));
        // Heart on fire: U+2764 U+FE0F U+200D U+1F525.
        assert!(!has_format_char("\u{2764}\u{FE0F}\u{200D}\u{1F525}"));
    }

    #[test]
    fn zwj_outside_an_emoji_sequence_is_hidden_text() {
        assert!(has_format_char("pay\u{200D}now"));
        assert!(has_format_char("\u{FE0F}\u{200D}\u{1F308}"));
    }

    #[test]
    fn cf_table_covers_known_format_characters() {
        assert!(is_format_char('\u{200B}'));
        assert!(is_format_char('\u{202E}'));
        assert!(is_format_char('\u{00AD}'));
        assert!(!is_format_char('a'));
        assert!(!is_format_char('\u{FE0F}'));
    }

    #[test]
    fn emoji_zwj_sequence_is_not_hidden_text() {
        let input = ScreeningInput {
            plain_parts: vec!["family \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} photo".into()],
            ..Default::default()
        };
        let outcome = screen(&input).unwrap();
        assert!(!outcome.families.contains(&Family::HiddenText));
    }

    #[test]
    fn from_message_extracts_plain_html_and_attachment() {
        let raw: &[u8] = b"From: a@example.com\r\n\
Subject: hello\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"B\"\r\n\
\r\n\
--B\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
plain body here\r\n\
--B\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>html body</p>\r\n\
--B\r\n\
Content-Type: application/pdf; name=\"report.pdf\"\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
\r\n\
PDFDATA\r\n\
--B--\r\n";
        let message = mail_parser::MessageParser::default()
            .parse(raw)
            .expect("raw message must parse");
        let input = ScreeningInput::from_message(&message);
        assert_eq!(input.subject, "hello");
        assert!(
            input
                .plain_parts
                .iter()
                .any(|part| part.contains("plain body here")),
            "plain parts: {:?}",
            input.plain_parts
        );
        assert!(
            input
                .html_parts
                .iter()
                .any(|part| part.contains("html body")),
            "html parts: {:?}",
            input.html_parts
        );
        assert!(
            input
                .attachments
                .iter()
                .any(|a| a.file_name.as_deref() == Some("report.pdf")),
            "attachments: {:?}",
            input.attachments
        );
    }

    #[test]
    fn from_message_html_only_yields_html_only_family() {
        let raw: &[u8] = b"From: a@example.com\r\n\
Subject: hello\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>hello there</p>\r\n";
        let message = mail_parser::MessageParser::default()
            .parse(raw)
            .expect("raw message must parse");
        let outcome = screen(&ScreeningInput::from_message(&message)).unwrap();
        assert_eq!(outcome.families, vec![Family::HtmlOnly]);
        assert_eq!(outcome.disposition, Disposition::DeliverMarked);
    }

    #[test]
    fn bare_cf_character_is_hidden_text() {
        let input = ScreeningInput {
            plain_parts: vec!["pay\u{200B}pal".into()],
            ..Default::default()
        };
        let outcome = screen(&input).unwrap();
        assert!(outcome.families.contains(&Family::HiddenText));
    }
}
