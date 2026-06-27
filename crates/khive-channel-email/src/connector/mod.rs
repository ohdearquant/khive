//! Transport-level connector types for SMTP and IMAP.

pub mod imap;
pub mod smtp;

use std::collections::HashMap;

use chrono::{DateTime, Utc};

pub use imap::ImapFetcher;
pub use smtp::SmtpSender;

/// A parsed RFC 822 address (addr-spec only, display name stripped).
///
/// Stored in lowercase for case-insensitive comparison per RFC 5321.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailAddress(String);

impl MailAddress {
    /// Parse a raw header value into a `MailAddress`.
    ///
    /// Strips display names and angle brackets; lowercases the result.
    /// Returns `None` if no valid addr-spec can be extracted.
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        // Handle "Display Name <addr@example.com>" form.
        if let Some(start) = trimmed.rfind('<') {
            if let Some(end) = trimmed[start..].find('>') {
                let addr = trimmed[start + 1..start + end].trim().to_lowercase();
                if addr.contains('@') {
                    return Some(Self(addr));
                }
            }
        }
        // Plain addr-spec.
        let lower = trimmed.to_lowercase();
        if lower.contains('@') {
            return Some(Self(lower));
        }
        None
    }

    /// Return the normalized addr-spec string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MailAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A raw email fetched from the IMAP server before enrichment.
#[derive(Debug, Clone)]
pub struct RawEmail {
    /// IMAP UID (opaque; used for SEEN marking).
    pub uid: u32,
    /// RFC 822 Message-ID header value. Used as the dedup external_id.
    pub message_id: Option<String>,
    /// Parsed sender address (addr-spec, lowercase).
    pub from: String,
    /// All recipient addresses.
    pub to: Vec<String>,
    /// Subject header value.
    pub subject: String,
    /// Date header parsed to UTC.
    pub date: Option<DateTime<Utc>>,
    /// Plain-text body.
    pub body_text: Option<String>,
    /// HTML body (informational; not stored in the KG note).
    pub body_html: Option<String>,
    /// All headers as a flat map (lowercase key, first-occurrence value).
    pub headers: HashMap<String, String>,
}

impl RawEmail {
    /// Return the value of the `X-Khive-Thread-ID` header if present.
    pub fn khive_thread_id(&self) -> Option<&str> {
        self.headers.get("x-khive-thread-id").map(|s| s.as_str())
    }

    /// Return the `In-Reply-To` header value if present.
    pub fn in_reply_to(&self) -> Option<&str> {
        self.headers.get("in-reply-to").map(|s| s.as_str())
    }

    /// Resolve the best available correlation key: `X-Khive-Thread-ID` first,
    /// then `In-Reply-To`.
    pub fn correlation(&self) -> Option<&str> {
        self.khive_thread_id().or_else(|| self.in_reply_to())
    }

    /// Return the best available body text.
    pub fn best_body(&self) -> String {
        self.body_text.clone().unwrap_or_default()
    }
}
