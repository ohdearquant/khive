use std::collections::BTreeMap;
use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};

/// The unsubscribe token format version carried as the first token byte.
const UNSUBSCRIBE_TOKEN_VERSION: u8 = 0x01;
/// A raw recipient key is one SHA-256 digest.
const RECIPIENT_KEY_BYTES: usize = 32;
/// The truncated HMAC tag length carried by a token.
const UNSUBSCRIBE_TAG_BYTES: usize = 16;
/// A token is `0x01 || key_id || recipient_key || tag`, base64url (no padding).
const UNSUBSCRIBE_TOKEN_BYTES: usize = 1 + 1 + RECIPIENT_KEY_BYTES + UNSUBSCRIBE_TAG_BYTES;
/// The placeholder the operator's footer must carry for the mailto instruction.
const UNSUBSCRIBE_PLACEHOLDER: &str = "{unsubscribe}";
/// A recipient-key secret and every token key must decode to at least this many bytes.
const MIN_KEY_BYTES: usize = 32;

const LIST_FOOTER_ENV: &str = "KHIVE_EMAIL_LIST_FOOTER_FILE";

const MIRROR_URL_ENV: &str = "KHIVE_EMAIL_SUPPRESSION_MIRROR_URL";
const MIRROR_TOKEN_ENV: &str = "KHIVE_EMAIL_SUPPRESSION_MIRROR_TOKEN";
const MIRROR_MAX_AGE_ENV: &str = "KHIVE_EMAIL_SUPPRESSION_MIRROR_MAX_AGE_SECS";

/// The remote suppression mirror (ADR-200 §6): the URL that answers the complete set of
/// suppressed recipient keys, the bearer credential that reads it, and how old the last
/// successful refresh may be before external admission refuses.
#[derive(Clone)]
pub struct SuppressionMirror {
    url: Arc<str>,
    credential: Arc<str>,
    max_age: std::time::Duration,
}

impl SuppressionMirror {
    /// The URL the delivery component reads the complete remote set from.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The bearer credential for the read. It is never written to a log or an error.
    pub fn credential(&self) -> &str {
        &self.credential
    }

    /// The age past which the last successful refresh no longer admits an external message.
    pub fn max_age(&self) -> std::time::Duration {
        self.max_age
    }
}

/// Boot-resolved recipient policy shared by comm admission and email delivery.
/// An absent policy permits queuing without claiming that delivery is configured.
#[derive(Clone)]
pub struct OutboundEmailPolicy {
    recipients: Option<Arc<[String]>>,
    external_senders: Option<Arc<[String]>>,
    external_daily_cap: Option<u64>,
    /// The recipient-key secret. Present only when external senders are configured; the derived
    /// key for an address is `HMAC-SHA256(K_rk, normalized address)` in lowercase hex.
    recipient_key_secret: Option<Arc<[u8]>>,
    /// Token keys by key id, ascending. Every retired id stays present so a link in an old
    /// message keeps verifying; the highest id mints new tokens.
    token_keys: Arc<BTreeMap<u8, Arc<[u8]>>>,
    /// The operator's external footer text, or the reason it is unavailable. `None` while
    /// external senders are unset, in which case no footer is ever consulted.
    external_footer: Option<Result<Arc<str>, Arc<str>>>,
    /// The separate list footer for external `traffic:list` messages. It never falls back to
    /// the default external footer; `None` only while external senders are unset.
    list_footer: Option<Result<Arc<str>, Arc<str>>>,
    /// The remote suppression mirror. `None` when it is not configured or external senders are
    /// unset.
    suppression_mirror: Option<SuppressionMirror>,
    external_admission: Arc<tokio::sync::Mutex<()>>,
}

impl Default for OutboundEmailPolicy {
    fn default() -> Self {
        Self {
            recipients: None,
            external_senders: None,
            external_daily_cap: None,
            recipient_key_secret: None,
            token_keys: Arc::default(),
            external_footer: None,
            list_footer: None,
            suppression_mirror: None,
            external_admission: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

/// HMAC-SHA256 (RFC 2104) over `sha2::Sha256`.
///
/// The 64-byte block is fixed for SHA-256. A key longer than the block is hashed first; a
/// shorter one is zero-padded to the block. Returns the full 32-byte MAC.
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= block[index];
        outer_pad[index] ^= block[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(data);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    let mut mac = [0u8; 32];
    mac.copy_from_slice(&outer.finalize());
    mac
}

/// The direction stored on one email suppression entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmailSuppressionScope {
    Outbound,
    Inbound,
}

/// The identity form stored on one email suppression entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmailSuppressionKind {
    Address,
    Domain,
    RecipientKey,
}

/// The matching fields needed by outbound admission and delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmailSuppression {
    pub scope: EmailSuppressionScope,
    pub kind: EmailSuppressionKind,
    pub value: String,
}

/// Check one already-normalized recipient against the suppression rows supplied by the caller.
///
/// Address normalization is shared with [`OutboundEmailPolicy`]; callers must use
/// `khive_types::email_address::normalize_email_recipient` before calling this function.
/// Domain comparison is exact, and recipient keys compare byte-for-byte.
pub fn outbound_email_is_suppressed(
    normalized_recipient: &str,
    recipient_key: Option<&str>,
    entries: &[EmailSuppression],
) -> bool {
    let recipient_domain = normalized_recipient
        .rsplit_once('@')
        .map(|(_, domain)| domain);
    entries.iter().any(|entry| {
        if entry.scope != EmailSuppressionScope::Outbound {
            return false;
        }
        match entry.kind {
            EmailSuppressionKind::Address => entry.value == normalized_recipient,
            EmailSuppressionKind::Domain => recipient_domain == Some(entry.value.as_str()),
            EmailSuppressionKind::RecipientKey => recipient_key == Some(entry.value.as_str()),
        }
    })
}

/// Check one already-normalized inbound sender address against the suppression rows supplied by
/// the caller. Only `inbound` entries can match, and only `address` and `domain` kinds can: an
/// inbound sender has no recipient key. Address comparison is exact after normalization, and
/// domain comparison is exact with no subdomain wildcard, so a `domain` entry for `example.com`
/// never blocks `user@sub.example.com`.
///
/// Callers must normalize the sender address with
/// `khive_types::email_address::normalize_email_recipient` before calling this function.
pub fn inbound_email_is_suppressed(normalized_sender: &str, entries: &[EmailSuppression]) -> bool {
    let sender_domain = normalized_sender.rsplit_once('@').map(|(_, domain)| domain);
    entries.iter().any(|entry| {
        if entry.scope != EmailSuppressionScope::Inbound {
            return false;
        }
        match entry.kind {
            EmailSuppressionKind::Address => entry.value == normalized_sender,
            EmailSuppressionKind::Domain => sender_domain == Some(entry.value.as_str()),
            EmailSuppressionKind::RecipientKey => false,
        }
    })
}

impl OutboundEmailPolicy {
    /// Install a normalized recipient set; an explicitly empty set denies all recipients.
    pub fn configured(recipients: Vec<String>) -> Result<Self, String> {
        let recipients = recipients
            .iter()
            .map(|value| {
                khive_types::email_address::normalize_email_recipient(value)
                    .ok_or_else(|| "outbound email policy contains an invalid address".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            recipients: Some(recipients.into()),
            ..Self::default()
        })
    }

    pub fn is_configured(&self) -> bool {
        self.recipients.is_some()
    }

    pub fn allows(&self, recipient: &str) -> bool {
        self.recipients.as_ref().is_none_or(|recipients| {
            khive_types::email_address::normalize_email_recipient(recipient)
                .is_some_and(|recipient| recipients.iter().any(|allowed| allowed == &recipient))
        })
    }

    pub fn allows_external_sender(&self, actor_id: &str) -> bool {
        self.external_senders
            .as_ref()
            .is_some_and(|senders| senders.iter().any(|sender| sender == actor_id))
    }

    pub fn has_external_senders(&self) -> bool {
        self.external_senders.is_some()
    }

    pub fn external_daily_cap(&self) -> Option<u64> {
        self.external_daily_cap
    }

    pub async fn lock_external_admission(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.external_admission).lock_owned().await
    }

    /// The recipient key a `recipient_key` suppression entry matches, derived on demand as
    /// `HMAC-SHA256(K_rk, normalized address)` in lowercase hex; the raw bytes are never stored.
    /// `None` when no recipient-key secret is configured (external senders unset).
    pub fn recipient_key(&self, normalized_recipient: &str) -> Option<String> {
        self.recipient_key_secret
            .as_deref()
            .map(|secret| hex_lower(&hmac_sha256(secret, normalized_recipient.as_bytes())))
    }

    /// The recipient key's raw 32 bytes, used as the token body.
    fn recipient_key_bytes(&self, normalized_recipient: &str) -> Option<[u8; RECIPIENT_KEY_BYTES]> {
        self.recipient_key_secret
            .as_deref()
            .map(|secret| hmac_sha256(secret, normalized_recipient.as_bytes()))
    }

    /// The footer text for external messages, or the reason it is unavailable. `None` while
    /// external senders are unset.
    pub fn external_footer(&self) -> Option<Result<&str, &str>> {
        self.external_footer.as_ref().map(|value| {
            value
                .as_ref()
                .map(|text| &**text)
                .map_err(|reason| &**reason)
        })
    }

    /// The separate footer for external list traffic, or the reason it is unavailable.
    /// `None` while external senders are unset; never consults the default footer.
    pub fn list_footer(&self) -> Option<Result<&str, &str>> {
        self.list_footer.as_ref().map(|value| {
            value
                .as_ref()
                .map(|text| &**text)
                .map_err(|reason| &**reason)
        })
    }

    /// Select the footer for an external send. List traffic never falls back to the default.
    pub fn external_footer_for_list(&self, is_list: bool) -> Option<Result<&str, &str>> {
        if is_list {
            self.list_footer()
        } else {
            self.external_footer()
        }
    }

    fn resolve_list_footer(&mut self, path: Option<&str>) {
        if self.has_external_senders() {
            self.list_footer = Some(read_list_footer(path));
        }
    }

    /// The remote suppression mirror, when one is configured (ADR-200 §6).
    pub fn suppression_mirror(&self) -> Option<&SuppressionMirror> {
        self.suppression_mirror.as_ref()
    }

    /// Mint the unsubscribe token for a normalized external recipient, or `None` when the
    /// recipient-key secret or any token key is unavailable. The highest configured key id mints.
    pub fn mint_unsubscribe_token(&self, normalized_recipient: &str) -> Option<String> {
        let recipient_key = self.recipient_key_bytes(normalized_recipient)?;
        let (&key_id, key) = self.token_keys.iter().next_back()?;
        Some(encode_unsubscribe_token(key_id, key, &recipient_key))
    }

    /// Verify an unsubscribe token and return the recipient key text it identifies, or `None`
    /// on any malformed input or tag mismatch. Retired key ids keep verifying.
    pub fn verify_unsubscribe_token(&self, token: &str) -> Option<String> {
        let decoded = URL_SAFE_NO_PAD.decode(token).ok()?;
        if decoded.len() != UNSUBSCRIBE_TOKEN_BYTES {
            return None;
        }
        if decoded[0] != UNSUBSCRIBE_TOKEN_VERSION {
            return None;
        }
        let key_id = decoded[1];
        let key = self.token_keys.get(&key_id)?;
        let (body, tag) = decoded.split_at(decoded.len() - UNSUBSCRIBE_TAG_BYTES);
        let expected = hmac_sha256(key, body);
        if !constant_time_eq(&expected[..UNSUBSCRIBE_TAG_BYTES], tag) {
            return None;
        }
        let recipient_key = &body[2..];
        Some(hex_lower(recipient_key))
    }

    /// Read only public policy inputs, without loading a connector or credentials.
    ///
    /// The unsubscribe key variables, the footer files and the suppression mirror variables are
    /// read only when external senders are configured; while the senders variable is unset none
    /// of them is consulted.
    pub fn from_env() -> Result<Self, String> {
        fn read(name: &str) -> Result<Option<String>, String> {
            match std::env::var(name) {
                Ok(value) => Ok(Some(value)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(std::env::VarError::NotUnicode(_)) => {
                    Err(format!("{name} must contain valid Unicode text"))
                }
            }
        }
        let external_senders = read("KHIVE_EMAIL_EXTERNAL_SENDERS")?;
        let external_daily_cap = read("KHIVE_EMAIL_EXTERNAL_DAILY_CAP")?;
        let explicit = read("KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS")?;
        let maintainer = read("KHIVE_EMAIL_MAINTAINER_ADDRESS")?;
        let (recipient_key_secret, token_keys) = if external_senders.is_some() {
            (
                read("KHIVE_EMAIL_UNSUBSCRIBE_RK_KEY")?,
                read("KHIVE_EMAIL_UNSUBSCRIBE_TOKEN_KEYS")?,
            )
        } else {
            (None, None)
        };
        let footer_file = if external_senders.is_some() {
            read("KHIVE_EMAIL_EXTERNAL_FOOTER_FILE")?
        } else {
            None
        };
        let list_footer_file = if external_senders.is_some() {
            read(LIST_FOOTER_ENV)?
        } else {
            None
        };
        let (mirror_url, mirror_token, mirror_max_age) = if external_senders.is_some() {
            (
                read(MIRROR_URL_ENV)?,
                read(MIRROR_TOKEN_ENV)?,
                read(MIRROR_MAX_AGE_ENV)?,
            )
        } else {
            (None, None, None)
        };
        let mut policy = Self::resolve_with_external(
            explicit.as_deref(),
            maintainer.as_deref(),
            external_senders.as_deref(),
            external_daily_cap.as_deref(),
            recipient_key_secret.as_deref(),
            token_keys.as_deref(),
            footer_file.as_deref(),
        )?;
        policy.resolve_list_footer(list_footer_file.as_deref());
        policy.suppression_mirror = parse_suppression_mirror(
            mirror_url.as_deref(),
            mirror_token.as_deref(),
            mirror_max_age.as_deref(),
        )?;
        Ok(policy)
    }

    #[cfg(test)]
    fn resolve(explicit: Option<&str>, maintainer: Option<&str>) -> Result<Self, String> {
        Self::resolve_with_external(explicit, maintainer, None, None, None, None, None)
    }

    /// Build a fully enabled policy for debug-build tests: recipients, the external sender
    /// set, the daily cap, fixed test key material and a footer carrying `{unsubscribe}`.
    #[cfg(any(test, debug_assertions))]
    #[doc(hidden)]
    pub fn configured_for_tests(
        recipients: Vec<String>,
        external_senders: Vec<String>,
        daily_cap: u64,
    ) -> Result<Self, String> {
        let token_keys = format!("0:{},1:{}", "11".repeat(32), "22".repeat(32));
        Self::configured_for_tests_with(
            recipients,
            external_senders,
            daily_cap,
            &"cc".repeat(32),
            &token_keys,
            Ok("Reply to unsubscribe: {unsubscribe}"),
        )
    }

    /// Build an enabled policy with explicit key material and footer state for tests that need
    /// to exercise configuration or footer unavailability.
    #[cfg(any(test, debug_assertions))]
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn configured_for_tests_with(
        recipients: Vec<String>,
        external_senders: Vec<String>,
        daily_cap: u64,
        recipient_key_secret_hex: &str,
        token_keys: &str,
        footer: Result<&str, &str>,
    ) -> Result<Self, String> {
        if daily_cap == 0 {
            return Err("KHIVE_EMAIL_EXTERNAL_DAILY_CAP must be a positive integer".into());
        }
        let external_senders = parse_external_senders(&external_senders.join(","))?;
        if external_senders.is_empty() {
            return Err("KHIVE_EMAIL_EXTERNAL_SENDERS must contain at least one actor id".into());
        }
        let mut policy = Self::configured(recipients)?;
        policy.external_senders = Some(external_senders.into());
        policy.external_daily_cap = Some(daily_cap);
        let secret = parse_hex_key("KHIVE_EMAIL_UNSUBSCRIBE_RK_KEY", recipient_key_secret_hex)?;
        policy.recipient_key_secret = Some(Arc::from(secret.as_slice()));
        policy.token_keys = Arc::new(parse_token_keys(token_keys)?);
        policy.external_footer = Some(footer.map(Arc::from).map_err(Arc::from));
        policy.resolve_list_footer(None);
        Ok(policy)
    }

    /// Replace the footer state on a test policy (available/unavailable) without touching keys.
    #[cfg(any(test, debug_assertions))]
    #[doc(hidden)]
    pub fn with_external_footer_for_tests(mut self, footer: Result<&str, &str>) -> Self {
        self.external_footer = Some(footer.map(Arc::from).map_err(Arc::from));
        self
    }

    /// Replace the list footer state on a test policy without changing the default footer.
    #[cfg(any(test, debug_assertions))]
    #[doc(hidden)]
    pub fn with_list_footer_for_tests(mut self, footer: Result<&str, &str>) -> Self {
        self.list_footer = Some(footer.map(Arc::from).map_err(Arc::from));
        self
    }

    /// Attach a remote suppression mirror to a test policy. Any URL is accepted, so a test can
    /// point the mirror at a local listener.
    #[cfg(any(test, debug_assertions))]
    #[doc(hidden)]
    pub fn with_suppression_mirror_for_tests(
        mut self,
        url: &str,
        credential: &str,
        max_age: std::time::Duration,
    ) -> Self {
        self.suppression_mirror = Some(SuppressionMirror {
            url: Arc::from(url),
            credential: Arc::from(credential),
            max_age,
        });
        self
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_with_external(
        explicit: Option<&str>,
        maintainer: Option<&str>,
        external_senders: Option<&str>,
        external_daily_cap: Option<&str>,
        recipient_key_secret: Option<&str>,
        token_keys: Option<&str>,
        footer_file: Option<&str>,
    ) -> Result<Self, String> {
        let senders = external_senders.map(parse_external_senders).transpose()?;
        let cap = match external_daily_cap {
            Some(value) => {
                let parsed = value.parse::<u64>().map_err(|_| {
                    "KHIVE_EMAIL_EXTERNAL_DAILY_CAP must be a positive integer".to_string()
                })?;
                if parsed == 0 {
                    return Err("KHIVE_EMAIL_EXTERNAL_DAILY_CAP must be a positive integer".into());
                }
                Some(parsed)
            }
            None if senders.is_some() => {
                return Err("KHIVE_EMAIL_EXTERNAL_DAILY_CAP is required when KHIVE_EMAIL_EXTERNAL_SENDERS is set".into());
            }
            None => None,
        };
        let mut policy = Self::resolve_recipients(explicit, maintainer)?;
        policy.external_daily_cap = cap;
        if let Some(senders) = senders {
            policy.external_senders = Some(senders.into());
            let recipient_key_secret = parse_hex_key(
                "KHIVE_EMAIL_UNSUBSCRIBE_RK_KEY",
                recipient_key_secret.ok_or_else(|| {
                    "KHIVE_EMAIL_UNSUBSCRIBE_RK_KEY is required when KHIVE_EMAIL_EXTERNAL_SENDERS is set".to_string()
                })?,
            )?;
            let token_keys = parse_token_keys(token_keys.ok_or_else(|| {
                "KHIVE_EMAIL_UNSUBSCRIBE_TOKEN_KEYS is required when KHIVE_EMAIL_EXTERNAL_SENDERS is set".to_string()
            })?)?;
            policy.recipient_key_secret = Some(Arc::from(recipient_key_secret.as_slice()));
            policy.token_keys = Arc::new(token_keys);
            policy.external_footer = Some(read_external_footer(footer_file));
            policy.resolve_list_footer(None);
        }
        Ok(policy)
    }

    fn resolve_recipients(
        explicit: Option<&str>,
        maintainer: Option<&str>,
    ) -> Result<Self, String> {
        if let Some(policy) = Self::explicit(explicit)? {
            // A present maintainer value is read the way the email connector reads it at
            // start, so a value the connector would refuse is refused here too, even
            // when the explicit list supplies the recipients. An unset one is not an error.
            if let Some(raw) = maintainer {
                Self::maintainer_primary(raw)?;
            }
            return Ok(policy);
        }
        Self::maintainer(maintainer, explicit.is_some())
    }

    fn explicit(raw: Option<&str>) -> Result<Option<Self>, String> {
        let recipients: Vec<String> = raw
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect();
        if recipients.is_empty() {
            return Ok(None);
        }
        Self::configured(recipients)
            .map(Some)
            .map_err(|_| "KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS contains an invalid address".into())
    }

    fn maintainer(raw: Option<&str>, explicit_was_set: bool) -> Result<Self, String> {
        let Some(raw) = raw else {
            if explicit_was_set {
                return Err(
                    "configured outbound email policy must contain at least one address".into(),
                );
            }
            return Ok(Self::default());
        };
        Self::configured(vec![Self::maintainer_primary(raw)?])
    }

    /// Validate a present maintainer value and return its first (primary) address:
    /// split on commas, trim, drop empty values, every remaining value must parse
    /// and at least one must remain.
    fn maintainer_primary(raw: &str) -> Result<String, String> {
        let addresses = raw
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                khive_types::email_address::normalize_email_recipient(value).ok_or_else(|| {
                    "KHIVE_EMAIL_MAINTAINER_ADDRESS contains an invalid address".to_string()
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let Some(primary) = addresses.into_iter().next() else {
            return Err("KHIVE_EMAIL_MAINTAINER_ADDRESS must contain at least one address".into());
        };
        Ok(primary)
    }
}

/// Parse the remote suppression mirror's variables (ADR-200 §6). All three unset means no
/// mirror. The URL requires the other two, and either of the other two without the URL is an
/// error, so a partly configured mirror is refused at start instead of reading as an absent one.
/// No error repeats the credential.
fn parse_suppression_mirror(
    url: Option<&str>,
    credential: Option<&str>,
    max_age: Option<&str>,
) -> Result<Option<SuppressionMirror>, String> {
    let Some(url) = url else {
        if credential.is_some() {
            return Err(format!(
                "{MIRROR_TOKEN_ENV} is set but {MIRROR_URL_ENV} is not"
            ));
        }
        if max_age.is_some() {
            return Err(format!(
                "{MIRROR_MAX_AGE_ENV} is set but {MIRROR_URL_ENV} is not"
            ));
        }
        return Ok(None);
    };
    let host = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("{MIRROR_URL_ENV} must be an https:// URL"))?;
    if host.is_empty()
        || host.starts_with('/')
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!(
            "{MIRROR_URL_ENV} must be an https:// URL with a host"
        ));
    }
    let credential = credential
        .ok_or_else(|| format!("{MIRROR_TOKEN_ENV} is required when {MIRROR_URL_ENV} is set"))?;
    if credential.is_empty() || !credential.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(format!(
            "{MIRROR_TOKEN_ENV} must be non-empty printable ASCII without spaces"
        ));
    }
    let max_age = max_age
        .ok_or_else(|| format!("{MIRROR_MAX_AGE_ENV} is required when {MIRROR_URL_ENV} is set"))?;
    let seconds = max_age
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| format!("{MIRROR_MAX_AGE_ENV} must be a positive integer"))?;
    Ok(Some(SuppressionMirror {
        url: Arc::from(url),
        credential: Arc::from(credential),
        max_age: std::time::Duration::from_secs(seconds),
    }))
}

fn parse_external_senders(raw: &str) -> Result<Vec<String>, String> {
    raw.split(',')
        .map(str::trim)
        .map(|actor_id| {
            if actor_id.is_empty() {
                return Err("KHIVE_EMAIL_EXTERNAL_SENDERS contains an empty actor id".to_string());
            }
            khive_types::Namespace::parse(actor_id).map_err(|_| {
                format!("KHIVE_EMAIL_EXTERNAL_SENDERS contains an invalid actor id {actor_id:?}")
            })?;
            Ok(actor_id.to_string())
        })
        .collect()
}

/// Decode a hex key for `name`, requiring at least [`MIN_KEY_BYTES`] bytes. The value is never
/// echoed in the error.
fn parse_hex_key(name: &str, raw: &str) -> Result<Vec<u8>, String> {
    let value = raw.trim().to_ascii_lowercase();
    if !value.len().is_multiple_of(2) || !khive_types::is_lowercase_hex(&value) {
        return Err(format!(
            "{name} must be hex-encoded key material of at least {MIN_KEY_BYTES} bytes"
        ));
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high = (pair[0] as char)
            .to_digit(16)
            .expect("validated lowercase hex");
        let low = (pair[1] as char)
            .to_digit(16)
            .expect("validated lowercase hex");
        bytes.push((high * 16 + low) as u8);
    }
    if bytes.len() < MIN_KEY_BYTES {
        return Err(format!(
            "{name} must be hex-encoded key material of at least {MIN_KEY_BYTES} bytes"
        ));
    }
    Ok(bytes)
}

/// Parse `KHIVE_EMAIL_UNSUBSCRIBE_TOKEN_KEYS`: comma-separated `id:key` pairs, `id` a decimal
/// integer 0-255, no duplicate ids, key material hex of at least [`MIN_KEY_BYTES`] bytes.
fn parse_token_keys(raw: &str) -> Result<BTreeMap<u8, Arc<[u8]>>, String> {
    const NAME: &str = "KHIVE_EMAIL_UNSUBSCRIBE_TOKEN_KEYS";
    let mut keys = BTreeMap::new();
    for pair in raw.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            return Err(format!("{NAME} contains an empty `id:key` pair"));
        }
        let Some((id_text, key_text)) = pair.split_once(':') else {
            return Err(format!("{NAME} entries must be `id:key` pairs"));
        };
        let id = id_text
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|id| *id <= u8::MAX as u16)
            .ok_or_else(|| format!("{NAME} key ids must be decimal integers 0-255"))?
            as u8;
        if keys.contains_key(&id) {
            return Err(format!("{NAME} contains a duplicate key id"));
        }
        let key = parse_hex_key(NAME, key_text)?;
        keys.insert(id, Arc::from(key.as_slice()));
    }
    if keys.is_empty() {
        return Err(format!("{NAME} must contain at least one `id:key` pair"));
    }
    Ok(keys)
}

/// Read the operator's external footer once at start-up. A missing, unreadable or
/// placeholder-less footer is not a start-up error; it is recorded as an unavailability reason
/// that refuses external admission.
fn read_external_footer(path: Option<&str>) -> Result<Arc<str>, Arc<str>> {
    read_footer_file(path, "KHIVE_EMAIL_EXTERNAL_FOOTER_FILE")
}

fn read_list_footer(path: Option<&str>) -> Result<Arc<str>, Arc<str>> {
    read_footer_file(path, LIST_FOOTER_ENV)
}

fn read_footer_file(path: Option<&str>, name: &str) -> Result<Arc<str>, Arc<str>> {
    let Some(path) = path else {
        return Err(Arc::<str>::from(format!("{name} is not set")));
    };
    let text = std::fs::read_to_string(path)
        .map_err(|_| Arc::<str>::from(format!("{name} could not be read")))?;
    if !text.contains(UNSUBSCRIBE_PLACEHOLDER) {
        return Err(Arc::<str>::from(format!(
            "{name} does not contain {UNSUBSCRIBE_PLACEHOLDER}"
        )));
    }
    Ok(Arc::from(text))
}

/// Lowercase hex of a byte slice (no `hex` crate dependency).
/// Encode an unsubscribe token: `0x01 || key_id || recipient_key || tag`, where the tag is the
/// first [`UNSUBSCRIBE_TAG_BYTES`] bytes of `HMAC-SHA256(key, 0x01 || key_id || recipient_key)`,
/// as base64url without padding.
fn encode_unsubscribe_token(
    key_id: u8,
    key: &[u8],
    recipient_key: &[u8; RECIPIENT_KEY_BYTES],
) -> String {
    let mut token = Vec::with_capacity(UNSUBSCRIBE_TOKEN_BYTES);
    token.push(UNSUBSCRIBE_TOKEN_VERSION);
    token.push(key_id);
    token.extend_from_slice(recipient_key);
    let tag = hmac_sha256(key, &token);
    token.extend_from_slice(&tag[..UNSUBSCRIBE_TAG_BYTES]);
    URL_SAFE_NO_PAD.encode(token)
}

fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Constant-time comparison over two equal-length byte slices (XOR-fold, no early return).
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_policy_and_requests_share_maintainer_normalization() {
        let policy =
            OutboundEmailPolicy::explicit(Some(" , First@Example.com , second@example.com,,"))
                .unwrap()
                .unwrap();
        assert!(policy.allows("First@Example.com"));
        assert!(policy.allows("second@example.com"));
        assert!(policy.allows("first@example.com"));
        assert!(policy.allows(" First@Example.com "));
        assert!(policy.allows("First <FIRST@EXAMPLE.COM>"));
        assert!(!policy.allows("other@example.com"));
        assert!(!policy.allows("not-an-address"));
        assert!(policy.is_configured());
        assert_eq!(
            policy.recipients.as_deref().unwrap(),
            ["first@example.com", "second@example.com"]
        );
        assert!(OutboundEmailPolicy::explicit(Some(" , , "))
            .unwrap()
            .is_none());
        assert!(OutboundEmailPolicy::explicit(Some("not-an-address")).is_err());
    }

    #[test]
    fn fallback_validates_all_maintainers_and_allows_only_normalized_primary() {
        let policy = OutboundEmailPolicy::maintainer(
            Some("Owner <Primary@Example.com>, second@example.com"),
            false,
        )
        .unwrap();
        assert!(policy.allows("primary@example.com"));
        assert!(policy.allows("Primary@Example.com"));
        assert!(policy.allows("Owner <PRIMARY@EXAMPLE.COM>"));
        assert!(!policy.allows("second@example.com"));
        assert!(
            OutboundEmailPolicy::maintainer(Some("primary@example.com,invalid"), false).is_err()
        );
        assert!(OutboundEmailPolicy::maintainer(Some(" , "), false).is_err());
        assert!(OutboundEmailPolicy::maintainer(None, true).is_err());
        assert!(
            OutboundEmailPolicy::maintainer(Some("primary@example.com"), true)
                .unwrap()
                .allows("PRIMARY@example.com")
        );
        assert!(OutboundEmailPolicy::maintainer(None, false)
            .unwrap()
            .allows("backlog@example.com"));
        assert!(!OutboundEmailPolicy::default().is_configured());
        let deny_all = OutboundEmailPolicy::configured(vec![]).unwrap();
        assert!(deny_all.is_configured());
        assert!(!deny_all.allows("backlog@example.com"));
        assert!(OutboundEmailPolicy::configured(vec!["invalid".into()]).is_err());
    }

    #[test]
    fn present_maintainer_is_validated_beside_an_explicit_list_and_an_unset_one_is_not() {
        let resolve = OutboundEmailPolicy::resolve;
        let explicit = Some("allowed@example.com");
        for maintainer in [
            "",
            " , ",
            "primary@example.com,invalid",
            "invalid,primary@example.com",
        ] {
            assert!(
                resolve(explicit, Some(maintainer)).is_err(),
                "maintainer {maintainer:?} beside a valid explicit list must be refused"
            );
        }
        let unset = resolve(explicit, None).unwrap();
        assert!(unset.is_configured());
        assert!(unset.allows("ALLOWED@example.com"));
        assert!(!unset.allows("primary@example.com"));
        let list = Some("Owner <Primary@Example.com>, second@example.com");
        let valid = resolve(explicit, list).unwrap();
        assert!(valid.allows("allowed@example.com"));
        assert!(!valid.allows("primary@example.com"));
        let blank = Some(" , ");
        let fallback = resolve(blank, list).unwrap();
        assert!(fallback.allows("primary@example.com"));
        assert!(!fallback.allows("second@example.com"));
        assert!(resolve(blank, None).is_err());
        assert!(resolve(Some("not-an-address"), None).is_err());
        assert!(resolve(None, Some("primary@example.com,invalid")).is_err());
        assert!(!resolve(None, None).unwrap().is_configured());
    }

    #[test]
    fn comparison_folds_ascii_case_only() {
        let policy = OutboundEmailPolicy::configured(vec!["kevin@example.com".into()]).unwrap();
        // U+212A KELVIN SIGN lowercases to ASCII `k` under Unicode rules; it is a different
        // address.
        assert!(!policy.allows("\u{212A}evin@example.com"));
        assert!(policy.allows("KEVIN@Example.COM"));
        assert!(policy.allows("kevin@example.com"));
    }

    #[test]
    fn configured_entries_and_requests_share_one_normalization() {
        // The entry and the request pass through the same function, so a non-ASCII letter in
        // an entry matches only its exact spelling while ASCII case still folds on both sides.
        let policy =
            OutboundEmailPolicy::configured(vec!["\u{212A}evin@Example.COM".into()]).unwrap();
        assert_eq!(
            policy.recipients.as_deref().unwrap(),
            ["\u{212A}evin@example.com"]
        );
        assert!(policy.allows("\u{212A}evin@EXAMPLE.com"));
        assert!(!policy.allows("kevin@example.com"));
        let accented =
            OutboundEmailPolicy::maintainer(Some("\u{C9}mile@Example.com"), false).unwrap();
        assert!(accented.allows("\u{C9}mile@example.COM"));
        assert!(!accented.allows("\u{E9}mile@example.com"));
    }

    #[test]
    fn suppression_matching_checks_exact_address_domain_and_recipient_key() {
        let address = [EmailSuppression {
            scope: EmailSuppressionScope::Outbound,
            kind: EmailSuppressionKind::Address,
            value: "a@example.com".into(),
        }];
        assert!(outbound_email_is_suppressed(
            "a@example.com",
            None,
            &address
        ));
        assert!(!outbound_email_is_suppressed(
            "b@example.com",
            None,
            &address
        ));

        let entries = [
            EmailSuppression {
                scope: EmailSuppressionScope::Outbound,
                kind: EmailSuppressionKind::Address,
                value: "a@example.com".into(),
            },
            EmailSuppression {
                scope: EmailSuppressionScope::Outbound,
                kind: EmailSuppressionKind::Domain,
                value: "x.com".into(),
            },
            EmailSuppression {
                scope: EmailSuppressionScope::Outbound,
                kind: EmailSuppressionKind::RecipientKey,
                value: "key-1".into(),
            },
            EmailSuppression {
                scope: EmailSuppressionScope::Inbound,
                kind: EmailSuppressionKind::Address,
                value: "b@example.com".into(),
            },
        ];

        assert!(outbound_email_is_suppressed(
            "a@example.com",
            None,
            &entries
        ));
        assert!(outbound_email_is_suppressed("b@x.com", None, &entries));
        assert!(!outbound_email_is_suppressed("b@sub.x.com", None, &entries));
        assert!(outbound_email_is_suppressed(
            "c@example.com",
            Some("key-1"),
            &entries
        ));
        assert!(!outbound_email_is_suppressed(
            "c@example.com",
            Some("key-2"),
            &entries
        ));
        assert!(!outbound_email_is_suppressed(
            "b@example.com",
            None,
            &entries
        ));
    }

    #[test]
    fn inbound_matching_checks_exact_address_and_domain_and_ignores_recipient_keys() {
        let entries = [
            EmailSuppression {
                scope: EmailSuppressionScope::Inbound,
                kind: EmailSuppressionKind::Address,
                value: "a@example.com".into(),
            },
            EmailSuppression {
                scope: EmailSuppressionScope::Inbound,
                kind: EmailSuppressionKind::Domain,
                value: "blocked.com".into(),
            },
            EmailSuppression {
                scope: EmailSuppressionScope::Inbound,
                kind: EmailSuppressionKind::RecipientKey,
                value: "key-1".into(),
            },
            // An outbound-only entry never blocks an inbound sender.
            EmailSuppression {
                scope: EmailSuppressionScope::Outbound,
                kind: EmailSuppressionKind::Address,
                value: "c@example.com".into(),
            },
        ];

        assert!(inbound_email_is_suppressed("a@example.com", &entries));
        assert!(!inbound_email_is_suppressed("b@example.com", &entries));
        assert!(inbound_email_is_suppressed("z@blocked.com", &entries));
        assert!(!inbound_email_is_suppressed("z@sub.blocked.com", &entries));
        assert!(!inbound_email_is_suppressed("c@example.com", &entries));
        assert!(!inbound_email_is_suppressed("key-1", &entries));
    }

    #[test]
    fn cloned_policy_keeps_the_same_immutable_recipient_set() {
        let policy = OutboundEmailPolicy::configured(vec!["allowed@example.com".into()]).unwrap();
        let clone = policy.clone();
        assert!(Arc::ptr_eq(
            policy.recipients.as_ref().unwrap(),
            clone.recipients.as_ref().unwrap()
        ));
    }

    fn rk() -> String {
        "cc".repeat(32) // 32 bytes of 0xcc
    }

    fn token_keys_hex() -> String {
        format!("0:{},1:{}", "11".repeat(32), "22".repeat(32))
    }

    fn enabled() -> OutboundEmailPolicy {
        OutboundEmailPolicy::configured_for_tests_with(
            vec!["allowed@example.com".into()],
            vec!["actor:sender".into()],
            5,
            &rk(),
            &token_keys_hex(),
            Ok("Reply to unsubscribe: {unsubscribe}"),
        )
        .unwrap()
    }

    fn unhex(hex: &str) -> Vec<u8> {
        hex.as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let high = (pair[0] as char).to_digit(16).unwrap();
                let low = (pair[1] as char).to_digit(16).unwrap();
                (high * 16 + low) as u8
            })
            .collect()
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231_vectors() {
        let case1 = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            hex_lower(&case1),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        let case2 = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex_lower(&case2),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let case6 = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex_lower(&case6),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn key_configuration_refuses_malformed_values_when_senders_are_set() {
        // Missing recipient-key secret.
        let missing_rk = OutboundEmailPolicy::resolve_with_external(
            None,
            Some("maintainer@example.com"),
            Some("actor:sender"),
            Some("5"),
            None,
            Some(&token_keys_hex()),
            None,
        )
        .err()
        .expect("start-up policy is refused");
        assert!(
            missing_rk.contains("KHIVE_EMAIL_UNSUBSCRIBE_RK_KEY"),
            "{missing_rk}"
        );

        // Missing token keys.
        let missing_tokens = OutboundEmailPolicy::resolve_with_external(
            None,
            Some("maintainer@example.com"),
            Some("actor:sender"),
            Some("5"),
            Some(&rk()),
            None,
            None,
        )
        .err()
        .expect("start-up policy is refused");
        assert!(
            missing_tokens.contains("KHIVE_EMAIL_UNSUBSCRIBE_TOKEN_KEYS"),
            "{missing_tokens}"
        );

        for (rk, tokens, needle) in [
            (
                rk(),
                "0:nothex".to_string(),
                "KHIVE_EMAIL_UNSUBSCRIBE_TOKEN_KEYS",
            ),
            (rk(), "0".to_string(), "KHIVE_EMAIL_UNSUBSCRIBE_TOKEN_KEYS"),
            (
                rk(),
                "0:abcd".to_string(),
                "KHIVE_EMAIL_UNSUBSCRIBE_TOKEN_KEYS",
            ),
            (
                rk(),
                format!("0:{},0:{}", "11".repeat(32), "22".repeat(32)),
                "KHIVE_EMAIL_UNSUBSCRIBE_TOKEN_KEYS",
            ),
            (
                rk(),
                format!("256:{}", "11".repeat(32)),
                "KHIVE_EMAIL_UNSUBSCRIBE_TOKEN_KEYS",
            ),
        ] {
            let error = OutboundEmailPolicy::resolve_with_external(
                None,
                Some("maintainer@example.com"),
                Some("actor:sender"),
                Some("5"),
                Some(rk.as_str()),
                Some(tokens.as_str()),
                None,
            )
            .err()
            .expect("start-up policy is refused");
            assert!(error.contains(needle), "{error}");
            assert!(
                !error.contains(&"11".repeat(32)),
                "key material leaked: {error}"
            );
        }

        // Short recipient-key secret (31 bytes) and non-hex secret.
        for rk in ["cc".repeat(31), "cc".repeat(16), "zz".repeat(32)] {
            let error = OutboundEmailPolicy::resolve_with_external(
                None,
                Some("maintainer@example.com"),
                Some("actor:sender"),
                Some("5"),
                Some(&rk),
                Some(&token_keys_hex()),
                None,
            )
            .err()
            .expect("start-up policy is refused");
            assert!(error.contains("KHIVE_EMAIL_UNSUBSCRIBE_RK_KEY"), "{error}");
            assert!(!error.contains(&rk), "key material leaked: {error}");
        }
    }

    #[test]
    fn key_variables_are_ignored_when_senders_are_unset() {
        // A malformed key value is never read while the senders variable is unset.
        let policy = OutboundEmailPolicy::resolve_with_external(
            None,
            Some("maintainer@example.com"),
            None,
            None,
            Some("not hex"),
            Some("garbage"),
            Some("/nonexistent/footer"),
        )
        .unwrap();
        assert!(!policy.has_external_senders());
        assert!(policy.recipient_key("a@example.com").is_none());
        assert!(policy.mint_unsubscribe_token("a@example.com").is_none());
        assert!(policy.external_footer().is_none());
    }

    #[test]
    fn recipient_key_is_normalized_hex_and_stable_across_case() {
        let policy = enabled();
        let key = policy.recipient_key("person@example.com").unwrap();
        assert_eq!(key.len(), 64);
        assert!(khive_types::is_lowercase_hex(&key));
        // Callers pass a normalized address (ADR-200 §5.1); normalization folds ASCII case.
        let other_case =
            khive_types::email_address::normalize_email_recipient("Person@Example.COM").unwrap();
        assert_eq!(key, policy.recipient_key(&other_case).unwrap());
        let secret = unhex(&rk());
        assert_eq!(key, hex_lower(&hmac_sha256(&secret, b"person@example.com")));
    }

    #[test]
    fn token_round_trips_and_rejects_tampering() {
        let policy = enabled();
        let token = policy.mint_unsubscribe_token("person@example.com").unwrap();
        let recipient_key = policy.recipient_key("person@example.com").unwrap();
        assert_eq!(
            policy.verify_unsubscribe_token(&token).as_deref(),
            Some(recipient_key.as_str())
        );

        // A flipped tag byte fails.
        let mut bytes = URL_SAFE_NO_PAD.decode(&token).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        assert!(policy
            .verify_unsubscribe_token(&URL_SAFE_NO_PAD.encode(&bytes))
            .is_none());

        // A wrong version byte fails.
        let mut bytes = URL_SAFE_NO_PAD.decode(&token).unwrap();
        bytes[0] = 0x02;
        assert!(policy
            .verify_unsubscribe_token(&URL_SAFE_NO_PAD.encode(&bytes))
            .is_none());

        // An unknown key id fails.
        let mut bytes = URL_SAFE_NO_PAD.decode(&token).unwrap();
        bytes[1] = 9;
        assert!(policy
            .verify_unsubscribe_token(&URL_SAFE_NO_PAD.encode(&bytes))
            .is_none());

        // A truncated token fails.
        assert!(policy.verify_unsubscribe_token("AAAA").is_none());
    }

    // A published test vector for the token format, shared with an independent verifier so that
    // a change to how either side mints or verifies fails a suite. The token key is test
    // material, never deployment key material.
    const VECTOR_KEY_ID: u8 = 3;
    const VECTOR_TOKEN_KEY_HEX: &str =
        "efaaa78a85da8a505a5278b5ad7d202a8b5028bb0b279c7a01f410c0ff84c09d";
    const VECTOR_RECIPIENT_KEY_HEX: &str =
        "cd5ac2457bd4442f53a23cfa9972cc3cdf3d0f1bcf1722bd5503ab20acfa3930";
    const VECTOR_TOKEN: &str =
        "AQPNWsJFe9REL1OiPPqZcsw83z0PG88XIr1VA6sgrPo5MFRThmznJxzUr1otDEzmcaE";
    const VECTOR_TOKEN_FLIPPED_TAG: &str =
        "AQPNWsJFe9REL1OiPPqZcsw83z0PG88XIr1VA6sgrPo5MFRThmznJxzUr1otDEzmcaA";

    #[test]
    fn shared_vector_mints_and_verifies_byte_for_byte() {
        let recipient_key: [u8; RECIPIENT_KEY_BYTES] = unhex(VECTOR_RECIPIENT_KEY_HEX)
            .try_into()
            .expect("the vector recipient key is 32 bytes");
        assert_eq!(
            encode_unsubscribe_token(VECTOR_KEY_ID, &unhex(VECTOR_TOKEN_KEY_HEX), &recipient_key),
            VECTOR_TOKEN,
            "minting the vector inputs must reproduce the vector token"
        );

        let policy = OutboundEmailPolicy::configured_for_tests_with(
            vec!["allowed@example.com".into()],
            vec!["actor:sender".into()],
            5,
            &rk(),
            &format!("{VECTOR_KEY_ID}:{VECTOR_TOKEN_KEY_HEX}"),
            Ok("Reply to unsubscribe: {unsubscribe}"),
        )
        .unwrap();
        assert_eq!(
            policy.verify_unsubscribe_token(VECTOR_TOKEN).as_deref(),
            Some(VECTOR_RECIPIENT_KEY_HEX),
            "the vector token verifies to the vector recipient key"
        );
        assert!(
            policy
                .verify_unsubscribe_token(VECTOR_TOKEN_FLIPPED_TAG)
                .is_none(),
            "the vector token with one tag bit flipped does not verify"
        );
    }

    #[test]
    fn retired_key_ids_keep_verifying_and_the_highest_id_mints() {
        let policy = enabled();
        // A token minted under the higher id (1) verifies.
        let token_high = policy.mint_unsubscribe_token("person@example.com").unwrap();
        let decoded = URL_SAFE_NO_PAD.decode(&token_high).unwrap();
        assert_eq!(decoded[1], 1, "minting must use the highest configured id");
        assert!(policy.verify_unsubscribe_token(&token_high).is_some());

        // Rebuild with a retired id 0 kept alongside the higher id 1.
        let retired = OutboundEmailPolicy::configured_for_tests_with(
            vec!["allowed@example.com".into()],
            vec!["actor:sender".into()],
            5,
            &rk(),
            &token_keys_hex(),
            Ok("footer {unsubscribe}"),
        )
        .unwrap();
        // A token that was minted under id 0 before rotation still verifies after id 1 exists.
        let mut body = Vec::new();
        body.push(0x01u8);
        body.push(0u8);
        body.extend_from_slice(&unhex(
            &retired.recipient_key("person@example.com").unwrap(),
        ));
        let tag = hmac_sha256(&unhex(&"11".repeat(32)), &body);
        body.extend_from_slice(&tag[..16]);
        let old_token = URL_SAFE_NO_PAD.encode(&body);
        assert_eq!(
            retired.verify_unsubscribe_token(&old_token).as_deref(),
            Some(
                retired
                    .recipient_key("person@example.com")
                    .unwrap()
                    .as_str()
            )
        );
    }

    #[test]
    fn footer_state_is_recorded_without_a_start_up_error() {
        let policy = OutboundEmailPolicy::configured_for_tests_with(
            vec!["allowed@example.com".into()],
            vec!["actor:sender".into()],
            5,
            &rk(),
            &token_keys_hex(),
            Err("KHIVE_EMAIL_EXTERNAL_FOOTER_FILE is not set"),
        )
        .unwrap();
        assert_eq!(
            policy.external_footer(),
            Some(Err("KHIVE_EMAIL_EXTERNAL_FOOTER_FILE is not set"))
        );
    }

    #[test]
    fn footer_file_must_exist_be_readable_and_carry_the_placeholder() {
        let missing = read_external_footer(Some("/nonexistent/khive-footer"));
        assert!(missing.is_err());
        let dir = std::env::temp_dir();
        let without = dir.join("khive-footer-without-placeholder.txt");
        std::fs::write(&without, "no placeholder here").unwrap();
        assert!(read_external_footer(without.to_str()).is_err());
        let with = dir.join("khive-footer-with-placeholder.txt");
        std::fs::write(&with, "Contact us.\n{unsubscribe}\n").unwrap();
        assert_eq!(
            read_external_footer(with.to_str()).unwrap().as_ref(),
            "Contact us.\n{unsubscribe}\n"
        );
        assert!(read_external_footer(None).is_err());
    }

    #[test]
    fn suppression_mirror_variables_parse_as_one_unit() {
        let token = "mirror-read-credential-value";
        assert!(parse_suppression_mirror(None, None, None)
            .unwrap()
            .is_none());
        let mirror = parse_suppression_mirror(
            Some("https://mirror.example.com/admin/email-suppression"),
            Some(token),
            Some("900"),
        )
        .unwrap()
        .expect("a complete mirror configuration parses");
        assert_eq!(
            mirror.url(),
            "https://mirror.example.com/admin/email-suppression"
        );
        assert_eq!(mirror.credential(), token);
        assert_eq!(mirror.max_age(), std::time::Duration::from_secs(900));

        let url = Some("https://mirror.example.com/x");
        for (url, credential, max_age, needle) in [
            (
                None,
                Some(token),
                None,
                "KHIVE_EMAIL_SUPPRESSION_MIRROR_TOKEN is set but",
            ),
            (
                None,
                None,
                Some("60"),
                "KHIVE_EMAIL_SUPPRESSION_MIRROR_MAX_AGE_SECS is set but",
            ),
            (
                url,
                None,
                Some("60"),
                "KHIVE_EMAIL_SUPPRESSION_MIRROR_TOKEN is required",
            ),
            (
                url,
                Some(token),
                None,
                "KHIVE_EMAIL_SUPPRESSION_MIRROR_MAX_AGE_SECS is required",
            ),
            (url, Some(token), Some("0"), "must be a positive integer"),
            (url, Some(token), Some("-5"), "must be a positive integer"),
            (url, Some(token), Some("ten"), "must be a positive integer"),
            (
                url,
                Some(""),
                Some("60"),
                "must be non-empty printable ASCII",
            ),
            (
                url,
                Some("two words"),
                Some("60"),
                "must be non-empty printable ASCII",
            ),
            (
                Some("http://mirror.example.com/x"),
                Some(token),
                Some("60"),
                "must be an https:// URL",
            ),
            (
                Some("HTTPS://mirror.example.com/x"),
                Some(token),
                Some("60"),
                "must be an https:// URL",
            ),
            (Some("https://"), Some(token), Some("60"), "with a host"),
            (
                Some("https:///path"),
                Some(token),
                Some("60"),
                "with a host",
            ),
            (
                Some("https://mirror.example.com/a b"),
                Some(token),
                Some("60"),
                "with a host",
            ),
        ] {
            let error = parse_suppression_mirror(url, credential, max_age)
                .err()
                .expect("a partial or malformed mirror configuration is refused");
            assert!(error.contains(needle), "{error}");
            assert!(!error.contains(token), "credential leaked: {error}");
        }
    }

    #[test]
    fn suppression_mirror_is_off_unless_configured() {
        assert!(enabled().suppression_mirror().is_none());
        assert!(OutboundEmailPolicy::default()
            .suppression_mirror()
            .is_none());
    }

    #[test]
    fn external_constructors_record_a_named_missing_list_footer() {
        let resolved = OutboundEmailPolicy::resolve_with_external(
            None,
            Some("maintainer@example.com"),
            Some("actor:sender"),
            Some("5"),
            Some(&rk()),
            Some(&token_keys_hex()),
            None,
        )
        .unwrap();
        let configured = OutboundEmailPolicy::configured_for_tests(
            vec!["allowed@example.com".into()],
            vec!["actor:sender".into()],
            5,
        )
        .unwrap();
        for policy in [resolved, configured, enabled()] {
            assert_eq!(
                policy.list_footer(),
                Some(Err("KHIVE_EMAIL_LIST_FOOTER_FILE is not set"))
            );
            assert_eq!(policy.external_footer_for_list(true), policy.list_footer());
        }
        let mut internal = OutboundEmailPolicy::default();
        internal.resolve_list_footer(Some("/nonexistent/unused-list-footer"));
        assert!(internal.list_footer().is_none());
        assert!(internal.external_footer_for_list(true).is_none());
    }

    #[test]
    fn list_footer_selection_never_falls_back_or_requires_the_default_footer() {
        let policy = enabled();
        assert_eq!(
            policy.external_footer_for_list(false),
            Some(Ok("Reply to unsubscribe: {unsubscribe}"))
        );
        assert_eq!(
            policy.external_footer_for_list(true),
            Some(Err("KHIVE_EMAIL_LIST_FOOTER_FILE is not set"))
        );
        let policy = policy
            .with_list_footer_for_tests(Ok("List notice: {unsubscribe}\n"))
            .with_external_footer_for_tests(Err("KHIVE_EMAIL_EXTERNAL_FOOTER_FILE is not set"));
        assert_eq!(
            policy.external_footer_for_list(true),
            Some(Ok("List notice: {unsubscribe}\n"))
        );
        assert_eq!(
            policy.external_footer_for_list(false),
            Some(Err("KHIVE_EMAIL_EXTERNAL_FOOTER_FILE is not set"))
        );
    }

    #[test]
    fn list_footer_file_validation_is_named_and_cached() {
        let dir = std::env::temp_dir().join(format!(
            "khive-list-footer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let unreadable = dir.join("directory-not-a-footer");
        std::fs::create_dir(&unreadable).unwrap();
        let without_placeholder = dir.join("without-placeholder.txt");
        std::fs::write(&without_placeholder, "operator-footer-content-sentinel").unwrap();
        let mut policy = enabled();
        for (path, reason) in [
            (None, "KHIVE_EMAIL_LIST_FOOTER_FILE is not set"),
            (
                Some(unreadable.to_str().unwrap()),
                "KHIVE_EMAIL_LIST_FOOTER_FILE could not be read",
            ),
            (
                Some(without_placeholder.to_str().unwrap()),
                "KHIVE_EMAIL_LIST_FOOTER_FILE does not contain {unsubscribe}",
            ),
        ] {
            policy.resolve_list_footer(path);
            assert_eq!(policy.list_footer(), Some(Err(reason)));
            assert_eq!(
                policy.external_footer(),
                Some(Ok("Reply to unsubscribe: {unsubscribe}"))
            );
        }
        let valid = dir.join("with-placeholder.txt");
        let original = "List contact.\n{unsubscribe}\n";
        std::fs::write(&valid, original).unwrap();
        policy.resolve_list_footer(valid.to_str());
        assert_eq!(policy.list_footer(), Some(Ok(original)));
        std::fs::write(&valid, "changed after startup").unwrap();
        std::fs::remove_file(&valid).unwrap();
        assert_eq!(policy.list_footer(), Some(Ok(original)));
        assert_eq!(policy.clone().list_footer(), Some(Ok(original)));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
