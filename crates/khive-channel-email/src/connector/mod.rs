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

    /// Authorization-equality against another address, applying Gmail's address
    /// canonicalization: for `gmail.com`/`googlemail.com` addresses, dots and a
    /// `+tag` suffix in the local part are insignificant and the two domains are
    /// equivalent (they route to the same mailbox). Non-Gmail domains compare by
    /// exact (already-lowercased) addr-spec, where dots ARE significant.
    ///
    /// This exists because clients such as Gmail emit the account's canonical
    /// From (often dotless) which need not string-match a dotted address a human
    /// configured. Exact equality would silently reject the maintainer's own mail.
    pub fn matches(&self, other: &MailAddress) -> bool {
        self.canonical() == other.canonical()
    }

    /// Canonical form used by [`matches`](Self::matches). Only Gmail addresses
    /// are rewritten; every other domain is returned as its stored addr-spec.
    fn canonical(&self) -> String {
        let Some((local, domain)) = self.0.split_once('@') else {
            return self.0.clone();
        };
        if domain == "gmail.com" || domain == "googlemail.com" {
            let base = local.split('+').next().unwrap_or(local);
            let dotless: String = base.chars().filter(|c| *c != '.').collect();
            format!("{dotless}@gmail.com")
        } else {
            self.0.clone()
        }
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
    /// IMAP UID (opaque; used for message identification).
    pub uid: u32,
    /// Stable dedup key: `imap:{host}:{uidvalidity}:{uid}`.
    ///
    /// Always set by the IMAP connector. Never empty. Used as the primary
    /// `external_id` in `comm.ingest`; derived from UIDVALIDITY and UID so
    /// dedup works even when a message has no `Message-ID` header.
    pub imap_external_id: String,
    /// All parsed sender addresses from the `From:` header (addr-spec, lowercase).
    ///
    /// The authorization check requires exactly one entry matching the configured
    /// maintainer address. Zero entries or more than one cause the message to be
    /// rejected as unauthorized before any note is written.
    pub from_addrs: Vec<String>,
    /// Parsed address from the `Sender:` header, if present (addr-spec, lowercase).
    ///
    /// When present, must also match the configured maintainer address.
    pub sender_addr: Option<String>,
    /// All recipient addresses from the `To:` header.
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

    /// Return this email's own `Message-ID` header value if present.
    ///
    /// Distinct from `imap_external_id` (the IMAP UIDVALIDITY/UID dedup key):
    /// this is the RFC 822 identifier the sending MUA minted, needed so a
    /// reply can set `In-Reply-To`/`References` for native MUA threading.
    pub fn message_id(&self) -> Option<&str> {
        self.headers.get("message-id").map(|s| s.as_str())
    }

    /// Return this email's own `References` header value if present, verbatim.
    ///
    /// RFC 5322 reply construction requires a reply's `References` to be the
    /// parent's existing `References` chain (this value) followed by the
    /// parent's `Message-ID`; capturing it here lets a later reply extend the
    /// full ancestor chain instead of truncating it to the immediate parent.
    pub fn references(&self) -> Option<&str> {
        self.headers.get("references").map(|s| s.as_str())
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
