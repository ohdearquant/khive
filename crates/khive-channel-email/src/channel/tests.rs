use super::*;
use crate::config::{EmailAuth, TrustAnchor};
use crate::connector::imap::{
    parse_raw_bytes, ImapConnector, ImapFetchPage, ImapFetcher, ImapProgress,
};
use crate::connector::smtp::{SmtpConnector, SmtpSender};
use khive_runtime::{KhiveRuntime, OutboundEmailPolicy};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The `authserv-id` this test suite's channels are configured to trust.
const TEST_AUTHSERV_ID: &str = "mx.example.com";

fn make_config(maintainer: &str) -> EmailChannelConfig {
    EmailChannelConfig {
        smtp_host: "smtp.example.com".to_string(),
        smtp_port: 587,
        imap_host: "imap.example.com".to_string(),
        imap_port: 993,
        imap_max_message_bytes: crate::config::DEFAULT_IMAP_MAX_MESSAGE_BYTES,
        imap_max_page_bytes: crate::config::DEFAULT_IMAP_MAX_PAGE_BYTES,
        username: "user@example.com".to_string(),
        mailbox: "user@example.com".to_string(),
        auth: EmailAuth::Basic {
            password: "secret".to_string(),
        },
        maintainer_addresses: vec![MailAddress::parse(maintainer).unwrap()],
        trust_anchor: TrustAnchor::AuthservId(TEST_AUTHSERV_ID.to_string()),
        quarantine_store: true,
        trust_exchange_internal: false,
        quarantine_max_retained: crate::config::DEFAULT_QUARANTINE_MAX_RETAINED,
    }
}

/// A trusted `Authentication-Results` header that authenticates cleanly
/// for `from_addr`'s own domain, so tests that are exercising allowlist
/// logic (not the auth gate) get an attributed baseline by default.
fn trusted_auth_header(from_addr: &str) -> String {
    let domain = from_addr
        .rsplit_once('@')
        .map(|(_, d)| d)
        .unwrap_or("example.com");
    format!("{TEST_AUTHSERV_ID}; dmarc=pass header.from={domain}")
}

struct RecordingSmtp {
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl SmtpConnector for RecordingSmtp {
    async fn deliver(
        &self,
        from: &str,
        to: &str,
        _subject: &str,
        _body: &str,
        _tid: Option<&str>,
        _message_id: Option<&str>,
        _in_reply_to: Option<&str>,
        _references: Option<&str>,
        _list_unsubscribe: Option<&str>,
    ) -> Result<(), ChannelError> {
        self.calls.lock().unwrap().push(format!("{from}->{to}"));
        Ok(())
    }
}

struct FixedImap {
    emails: Vec<RawEmail>,
}

#[async_trait]
impl ImapConnector for FixedImap {
    async fn fetch_page(
        &self,
        _since: DateTime<Utc>,
        _limit: usize,
        _progress: crate::connector::imap::ImapProgress,
    ) -> Result<ImapFetchPage, ChannelError> {
        Ok(ImapFetchPage {
            emails: self
                .emails
                .clone()
                .into_iter()
                .map(|e| SelectedMessage::Email(Box::new(e)))
                .collect(),
            next_progress: crate::connector::imap::ImapProgress::default(),
        })
    }
}

/// Build a RawEmail with a single-address From, a stable IMAP external ID,
/// and a trusted, self-aligned `Authentication-Results` header -- so
/// callers that are exercising allowlist logic (not the auth gate itself)
/// get an attributed baseline by default.
fn make_email(from_addr: &str, imap_id: &str) -> RawEmail {
    RawEmail {
        uid: 1,
        raw_bytes: Vec::new(),
        imap_external_id: imap_id.to_string(),
        from_addrs: vec![from_addr.to_string()],
        sender_addr: None,
        to: vec!["me@example.com".to_string()],
        subject: "Hello".to_string(),
        date: None,
        body_text: Some("body text".to_string()),
        body_html: None,
        headers: HashMap::new(),
        authentication_results: vec![trusted_auth_header(from_addr)],
    }
}

/// Build a RawEmail with an explicit From address list and a trusted
/// `Authentication-Results` header (aligned to the first From address, or
/// to `example.com` when the list is empty).
fn make_email_with_from_addrs(from_addrs: Vec<String>, imap_id: &str) -> RawEmail {
    let auth_header = from_addrs
        .first()
        .map(|addr| trusted_auth_header(addr))
        .unwrap_or_else(|| format!("{TEST_AUTHSERV_ID}; dmarc=pass header.from=example.com"));
    RawEmail {
        uid: 1,
        raw_bytes: Vec::new(),
        imap_external_id: imap_id.to_string(),
        from_addrs,
        sender_addr: None,
        to: vec!["me@example.com".to_string()],
        subject: "Hello".to_string(),
        date: None,
        body_text: Some("body text".to_string()),
        body_html: None,
        headers: HashMap::new(),
        authentication_results: vec![auth_header],
    }
}

fn build_channel(maintainer: &str, emails: Vec<RawEmail>) -> EmailChannel {
    build_channel_from(make_config(maintainer), emails)
}

fn build_channel_from(config: EmailChannelConfig, emails: Vec<RawEmail>) -> EmailChannel {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let smtp = SmtpSender::with_connector(RecordingSmtp {
        calls: calls.clone(),
    });
    let imap = ImapFetcher::with_connector(FixedImap { emails });
    EmailChannel::with_connectors(config, smtp, imap)
}

fn make_config_multi(maintainers: &[&str]) -> EmailChannelConfig {
    let mut config = make_config(maintainers[0]);
    config.maintainer_addresses = maintainers
        .iter()
        .map(|m| MailAddress::parse(m).unwrap())
        .collect();
    config
}

// --- Basic trait ---

#[test]
fn channel_reports_its_configured_quarantine_retention_limit() {
    let mut config = make_config("maintainer@example.com");
    config.quarantine_max_retained = 7;
    let ch = build_channel_from(config, vec![]);
    assert_eq!(ch.quarantine_retention_limit(), Some(7));
}

#[test]
fn kind_is_email() {
    let ch = build_channel("maintainer@example.com", vec![]);
    assert_eq!(ch.kind(), "email");
}

#[tokio::test]
async fn same_imap_uid_in_two_accounts_has_distinct_new_keys_and_shared_legacy_key() {
    let raw = "imap:mail.example.com:17:42";
    let mut config_a = make_config("maintainer@example.com");
    config_a.mailbox = "A@Example.com".to_string();
    let mut config_b = make_config("maintainer@example.com");
    config_b.mailbox = "b@example.com".to_string();
    let a = build_channel_from(config_a, vec![]);
    let b = build_channel_from(config_b, vec![]);
    let (a_env, _) = a
        .disposition(
            vec![SelectedMessage::Email(Box::new(make_email(
                "maintainer@example.com",
                raw,
            )))],
            None,
        )
        .await
        .unwrap();
    let (b_env, _) = b
        .disposition(
            vec![SelectedMessage::Email(Box::new(make_email(
                "maintainer@example.com",
                raw,
            )))],
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        a_env[0].external_id.as_deref(),
        Some("imap:mail.example.com:a@example.com:17:42")
    );
    assert_eq!(
        b_env[0].external_id.as_deref(),
        Some("imap:mail.example.com:b@example.com:17:42")
    );
    assert_ne!(a_env[0].external_id, b_env[0].external_id);
    assert_eq!(a_env[0].legacy_external_id.as_deref(), Some(raw));
    assert_eq!(b_env[0].legacy_external_id.as_deref(), Some(raw));
}

// --- Authorization: authorized sender ---

#[tokio::test]
async fn authorized_sender_produces_envelope() {
    let ch = build_channel(
        "maintainer@example.com",
        vec![make_email("maintainer@example.com", "imap:test:0:1")],
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    // external_id is now always the stable IMAP key, not Message-ID.
    assert_eq!(
        envs[0].external_id.as_deref(),
        Some("imap:test:user@example.com:0:1")
    );
    assert_eq!(envs[0].legacy_external_id.as_deref(), Some("imap:test:0:1"));
    assert_eq!(envs[0].from, "email:maintainer@example.com");
}

#[test]
fn spaced_auth_results_passes_attribution_gate() {
    let mut email = make_email("user@example.com", "imap:test:1:7");
    email.authentication_results =
        vec!["mx.example.com; dmarc = pass header . from = example.com".to_string()];
    let channel = build_channel("user@example.com", vec![]);
    assert_eq!(channel.evaluate_auth(&email), Ok(()));
}

/// The gate's attribution as a comparable label (`EmailAttribution` has no `PartialEq`).
fn gate_outcome(
    channel: &EmailChannel,
    email: &RawEmail,
    inbound: Option<&InboundSuppression>,
) -> Result<&'static str, QuarantineReason> {
    channel
        .gate(email, inbound)
        .map(|attribution| match attribution {
            EmailAttribution::Maintainer => "maintainer",
            EmailAttribution::External => "external",
        })
}

const EXCHANGE_INTERNAL: &[u8] = b"X-MS-Exchange-Organization-AuthAs: Internal\r\n\r\n";

/// A message carrying no `Authentication-Results` or ARC header, with the given raw header block.
fn exchange_email(from: &str, sender: Option<&str>, raw: &[u8]) -> RawEmail {
    let mut email = make_email(from, "imap:test:1:9");
    email.authentication_results.clear();
    email.sender_addr = sender.map(str::to_string);
    email.raw_bytes = raw.to_vec();
    email
}

/// A channel whose mailbox is `user@example.com` and whose maintainer allowlist also holds an
/// address on another domain and one on a subdomain, so the domain arms below refuse on the
/// domain rule alone and never on the allowlist.
fn exchange_config(enabled: bool) -> EmailChannelConfig {
    let mut config = make_config_multi(&[
        "owner@example.com",
        "owner@outside.example",
        "owner@sub.example.com",
    ]);
    config.trust_exchange_internal = enabled;
    config
}

#[test]
fn exchange_internal_anchor_admits_same_domain_maintainer() {
    let channel = build_channel_from(exchange_config(true), vec![]);
    let email = exchange_email(
        "owner@example.com",
        None,
        b"From: owner@example.com\r\nX-MS-Exchange-Organization-AuthAs: Internal\r\n\r\nbody",
    );
    assert_eq!(gate_outcome(&channel, &email, None), Ok("maintainer"));

    let folded = exchange_email(
        "owner@example.com",
        None,
        b"X-MS-Exchange-Organization-AuthAs:\r\n Internal\r\n\r\n",
    );
    assert_eq!(
        gate_outcome(&channel, &folded, None),
        Ok("maintainer"),
        "folded value"
    );

    let lower = exchange_email(
        "owner@example.com",
        None,
        b"x-ms-exchange-organization-authas: internal\r\n\r\n",
    );
    assert_eq!(
        gate_outcome(&channel, &lower, None),
        Ok("maintainer"),
        "case-insensitive"
    );
}

#[test]
fn exchange_internal_anchor_refuses_each_missing_condition() {
    let internal_then_anonymous: &[u8] = b"X-MS-Exchange-Organization-AuthAs: Internal\r\nX-MS-Exchange-Organization-AuthAs: Anonymous\r\n\r\n";
    let anonymous_then_internal: &[u8] = b"X-MS-Exchange-Organization-AuthAs: Anonymous\r\nX-MS-Exchange-Organization-AuthAs: Internal\r\n\r\n";
    let cases: &[(&str, bool, &str, Option<&str>, &[u8])] = &[
        (
            "opt-in off",
            false,
            "owner@example.com",
            None,
            EXCHANGE_INTERNAL,
        ),
        (
            "maintainer on another domain with a forged stamp",
            true,
            "owner@outside.example",
            None,
            EXCHANGE_INTERNAL,
        ),
        (
            "maintainer on a subdomain of the mailbox domain",
            true,
            "owner@sub.example.com",
            None,
            EXCHANGE_INTERNAL,
        ),
        ("stamp absent", true, "owner@example.com", None, b"\r\n"),
        (
            "stamp Anonymous",
            true,
            "owner@example.com",
            None,
            b"X-MS-Exchange-Organization-AuthAs: Anonymous\r\n\r\n",
        ),
        (
            "two stamps, Internal first",
            true,
            "owner@example.com",
            None,
            internal_then_anonymous,
        ),
        (
            "two stamps, Anonymous first",
            true,
            "owner@example.com",
            None,
            anonymous_then_internal,
        ),
        (
            "From not a maintainer",
            true,
            "other@example.com",
            None,
            EXCHANGE_INTERNAL,
        ),
        (
            "maintainer From with a non-maintainer Sender",
            true,
            "owner@example.com",
            Some("other@example.com"),
            EXCHANGE_INTERNAL,
        ),
        (
            "lookalike header name",
            true,
            "owner@example.com",
            None,
            b"X-MS-Exchange-Organization-AuthAsX: Internal\r\n\r\n",
        ),
        (
            "stamp after the header block",
            true,
            "owner@example.com",
            None,
            b"Subject: hi\r\n\r\nX-MS-Exchange-Organization-AuthAs: Internal\r\n",
        ),
    ];
    // Every arm runs, and a failure lists each arm that admitted, so a mutation shows its reach.
    let mut admitted = Vec::new();
    for &(label, enabled, from, sender, raw) in cases {
        let channel = build_channel_from(exchange_config(enabled), vec![]);
        let email = exchange_email(from, sender, raw);
        let outcome = gate_outcome(&channel, &email, None);
        if outcome != Err(QuarantineReason::AuthAbsent) {
            admitted.push(format!("{label}: {outcome:?}"));
        }
    }

    let mut two_from = exchange_email("owner@example.com", None, EXCHANGE_INTERNAL);
    two_from.from_addrs.push("owner@example.com".to_string());
    let channel = build_channel_from(exchange_config(true), vec![]);
    let outcome = gate_outcome(&channel, &two_from, None);
    if outcome != Err(QuarantineReason::AuthAbsent) {
        admitted.push(format!("two From addresses: {outcome:?}"));
    }
    assert!(
        admitted.is_empty(),
        "arms not refused as auth-absent: {admitted:#?}"
    );
}

#[test]
fn exchange_internal_anchor_leaves_authentication_results_reasons_unchanged() {
    let channel = build_channel_from(exchange_config(true), vec![]);
    let mut email = exchange_email("owner@example.com", None, EXCHANGE_INTERNAL);
    email.authentication_results =
        vec!["mx.other.example; dmarc=pass header.from=example.com".to_string()];
    assert_eq!(
        gate_outcome(&channel, &email, None),
        Err(QuarantineReason::AuthUntrusted)
    );

    email.authentication_results = vec![format!(
        "{TEST_AUTHSERV_ID}; dmarc=fail header.from=example.com"
    )];
    assert_eq!(
        gate_outcome(&channel, &email, None),
        Err(QuarantineReason::DmarcFail)
    );
}

#[test]
fn exchange_internal_anchor_never_attributes_external() {
    let inbound: InboundSuppression = Arc::new(|_| false);
    let channel = build_channel_from(exchange_config(true), vec![])
        .with_external_host(Arc::new(RoutingHost {
            enabled: true,
            ..Default::default()
        }))
        .with_external_inbound_config(ExternalInboundConfig {
            routes: crate::InboundRoutes::parse("colleague@example.com=lambda:colleague").unwrap(),
            actor: "lambda:recipient".into(),
        });

    // Control: the same non-maintainer sender with a trusted, aligned header reaches step 6
    // and is attributed External on this channel.
    let control = make_email("colleague@example.com", "imap:test:1:10");
    assert_eq!(
        gate_outcome(&channel, &control, Some(&inbound)),
        Ok("external")
    );

    let email = exchange_email("colleague@example.com", None, EXCHANGE_INTERNAL);
    assert_eq!(
        gate_outcome(&channel, &email, Some(&inbound)),
        Err(QuarantineReason::AuthAbsent)
    );
}

#[tokio::test]
async fn poll_extracts_wire_message_id_from_headers() {
    let mut email = make_email("maintainer@example.com", "imap:test:0:1");
    email
        .headers
        .insert("message-id".to_string(), "wire-abc@example.com".to_string());
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].wire_message_id.as_deref(),
        Some("wire-abc@example.com"),
        "poll must surface the inbound email's own Message-ID as wire_message_id"
    );
}

#[tokio::test]
async fn poll_wire_message_id_absent_when_no_header() {
    let ch = build_channel(
        "maintainer@example.com",
        vec![make_email("maintainer@example.com", "imap:test:0:1")],
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].wire_message_id, None,
        "no Message-ID header must leave wire_message_id unset, not fabricated"
    );
}

#[tokio::test]
async fn poll_extracts_wire_references_from_headers() {
    let mut email = make_email("maintainer@example.com", "imap:test:0:1");
    email.headers.insert(
        "references".to_string(),
        "<grandparent1@example.com> <parent123@example.com>".to_string(),
    );
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].wire_references.as_deref(),
        Some("<grandparent1@example.com> <parent123@example.com>"),
        "poll must surface the inbound email's own References chain as wire_references"
    );
}

#[tokio::test]
async fn poll_preserves_multi_id_wire_references_through_real_parse() {
    // Regression: connector::imap::parse_raw_bytes must not drop a multi-id
    // References chain (mail-parser's `HeaderValue::TextList`) -- exercise the
    // REAL byte-parsing path here, not a hand-built RawEmail.headers map, since
    // that hand-built shortcut is exactly what let the live-path bug through.
    let raw = b"Authentication-Results: mx.example.com; dmarc=pass header.from=example.com\r\n\
                From: maintainer@example.com\r\n\
                To: me@example.com\r\n\
                Subject: Multi-id References test\r\n\
                References: <grandparent1@example.com> <grandparent2@example.com>\r\n\
                \r\n\
                body text";
    let email =
        parse_raw_bytes(1, raw, "imap.example.com", 4242).expect("valid RFC 822 bytes must parse");
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].wire_references.as_deref(),
        Some("grandparent1@example.com grandparent2@example.com"),
        "both ancestor ids parsed from raw bytes must survive into wire_references; \
         got envs={envs:?}"
    );
    let replay = envs[0]
        .quarantine_replay
        .as_ref()
        .expect("parsed email must retain in-memory replay context");
    assert_eq!(
        replay.bytes, raw,
        "email replay context must preserve the byte-exact RFC 822 payload"
    );
    assert_eq!(
        replay.notification_to, "email:maintainer@example.com",
        "content refusal notifications must route to the configured maintainer"
    );
}

#[tokio::test]
async fn poll_wire_references_absent_when_no_header() {
    let ch = build_channel(
        "maintainer@example.com",
        vec![make_email("maintainer@example.com", "imap:test:0:1")],
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].wire_references, None,
        "no References header must leave wire_references unset, not fabricated"
    );
}

#[tokio::test]
async fn normalized_addr_spec_is_accepted() {
    // IMAP parsing strips display names before channel.rs sees the address.
    // from_addrs already contains the bare addr-spec.
    let ch = build_channel(
        "maintainer@example.com",
        vec![make_email("maintainer@example.com", "imap:test:0:1")],
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1, "plain addr-spec must be accepted");
}

#[tokio::test]
async fn raw_sender_allowlist_folds_only_ascii_case() {
    for (case, maintainer, from, sender, accepted) in [
        (
            "ASCII case variant",
            "Kevin@Example.COM",
            "KEVIN@EXAMPLE.COM",
            Some("KeViN@Example.Com"),
            true,
        ),
        (
            "Kelvin sign in From",
            "kevin@example.com",
            "\u{212a}evin@example.com",
            None,
            false,
        ),
        (
            "Kelvin sign in Sender",
            "kevin@example.com",
            "kevin@example.com",
            Some("\u{212a}evin@example.com"),
            false,
        ),
        (
            "Kelvin sign in configured maintainer",
            "Maintainer <\u{212a}evin@example.com>",
            "kevin@example.com",
            None,
            false,
        ),
        (
            "Exact non-ASCII mailbox with ASCII case variant",
            "Maintainer <\u{212a}EVIN@Example.Com>",
            "\u{212a}evin@example.com",
            Some("\u{212a}EVIN@EXAMPLE.COM"),
            true,
        ),
        (
            "Non-ASCII case variant",
            "\u{00c9}ric@example.com",
            "\u{00e9}ric@example.com",
            None,
            false,
        ),
    ] {
        let sender_header = sender
            .map(|address| format!("Sender: Delegate <{address}>\r\n"))
            .unwrap_or_default();
        let raw = format!(
            "From: Maintainer <{from}>\r\n\
             {sender_header}\
             To: inbox@example.com\r\n\
             Authentication-Results: {TEST_AUTHSERV_ID}; dmarc=pass header.from=example.com\r\n\
             X-Khive-Thread-ID: existing-thread\r\n\
             Subject: Sender identity\r\n\r\nbody"
        );
        let email = parse_raw_bytes(42, raw.as_bytes(), "imap.example.com", 9)
            .expect("raw SMTPUTF8 fixture must parse");
        let channel = build_channel(maintainer, vec![email.clone()]);
        assert!(
            channel.evaluate_auth(&email).is_ok(),
            "{case}: auth baseline"
        );
        let envelopes = channel.poll(Utc::now()).await.unwrap();
        assert_eq!(envelopes.len(), 1, "{case}: preserve the message");
        let envelope = &envelopes[0];
        if accepted {
            assert_eq!(
                envelope.from,
                format!("email:{}", from.to_ascii_lowercase()),
                "{case}"
            );
            assert!(!envelope.metadata.contains_key("quarantined"), "{case}");
            assert_eq!(
                envelope.correlation_external_id.as_deref(),
                Some("existing-thread"),
                "{case}"
            );
        } else {
            assert_eq!(envelope.from, EMAIL_QUARANTINE_SENDER, "{case}");
            assert_eq!(
                envelope
                    .metadata
                    .get("quarantine_reason")
                    .map(String::as_str),
                Some("off-allowlist"),
                "{case}"
            );
            assert_eq!(
                envelope.metadata.get("quarantined").map(String::as_str),
                Some("true"),
                "{case}"
            );
            assert_eq!(
                envelope.correlation_external_id, None,
                "{case}: quarantine cannot inherit thread context"
            );
        }
    }
}

// --- Authorization: rejected senders (Fix 1) ---

#[tokio::test]
async fn unauthorized_sender_with_valid_auth_is_quarantined_off_allowlist() {
    // Domain authentication passes (attacker@example.com's own AR header is
    // trusted and aligned), but the sender is not on the maintainer
    // allowlist -- must be quarantined with reason off-allowlist, never
    // attributed to the (non-maintainer) sender or silently dropped.
    let ch = build_channel(
        "maintainer@example.com",
        vec![make_email("attacker@example.com", "imap:test:0:1")],
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1, "must be stored as quarantine, not dropped");
    assert_eq!(
        envs[0].from, EMAIL_QUARANTINE_SENDER,
        "quarantine sender prefix invariant: `email:` keeps the alert visible to prefix-keyed consumers"
    );
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("off-allowlist")
    );
    assert_eq!(
        envs[0].metadata.get("quarantined").map(String::as_str),
        Some("true")
    );
}

#[tokio::test]
async fn gmail_dot_variants_authorize_against_dotted_maintainer() {
    // Maintainer configured with a dotted Gmail; the client may deliver the
    // dotless canonical form, a googlemail alias, or a +tag. All are the same
    // Gmail mailbox and must authorize.
    for from in [
        "samrivera@gmail.com",
        "sam.rivera@gmail.com",
        "samrivera@googlemail.com",
        "sam.rivera+khive@gmail.com",
    ] {
        let ch = build_channel(
            "sam.rivera@gmail.com",
            vec![make_email(from, "imap:test:0:1")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1, "gmail variant {from} must authorize");
    }
}

#[tokio::test]
async fn non_gmail_dots_remain_significant() {
    // Dot-insensitivity is a Gmail-only rule; other providers treat dots as
    // significant, so a dotted variant of a non-Gmail maintainer fails the
    // allowlist half of the gate (quarantined, not attributed).
    let ch = build_channel(
        "sam.rivera@outlook.com",
        vec![make_email("samrivera@outlook.com", "imap:test:0:1")],
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_ne!(
        envs[0].from, "email:samrivera@outlook.com",
        "non-gmail dot-variant must NOT authorize"
    );
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("off-allowlist")
    );
}

#[tokio::test]
async fn allowlist_authorizes_any_configured_maintainer() {
    // Multiple maintainers (e.g. a Gmail and an Outlook) may be registered;
    // a From matching any entry authorizes.
    let config = make_config_multi(&["primary@gmail.com", "second@outlook.com"]);
    let ch = build_channel_from(
        config,
        vec![make_email("second@outlook.com", "imap:test:0:1")],
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1, "any allowlisted maintainer must authorize");
}

#[tokio::test]
async fn multi_from_addresses_rejected() {
    // RFC 5322 permits multiple From addresses; we treat it as unauthorized
    // (quarantined off-allowlist, since domain auth on the first address passes).
    let ch = build_channel(
        "maintainer@example.com",
        vec![make_email_with_from_addrs(
            vec![
                "maintainer@example.com".to_string(),
                "attacker@example.com".to_string(),
            ],
            "imap:test:0:1",
        )],
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(
        envs.len(),
        1,
        "multi-From message must be quarantined, not dropped"
    );
    assert_eq!(
        envs[0].from, EMAIL_QUARANTINE_SENDER,
        "quarantine sender prefix invariant: `email:` keeps the alert visible to prefix-keyed consumers"
    );
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("off-allowlist")
    );
}

#[tokio::test]
async fn empty_from_list_rejected() {
    // With no From address there is no domain to align dmarc's
    // header.from against, so `dmarc_pass_aligned` (hardening #3, ADR-056
    // Amendment 2026-07-03) now correctly fails alignment and the
    // attribution gate catches this at the auth leg (dmarc-fail) before
    // ever reaching the sender allowlist -- still quarantined, just a
    // stricter (and more accurate) reason than the pre-hardening
    // off-allowlist path.
    let ch = build_channel(
        "maintainer@example.com",
        vec![make_email_with_from_addrs(vec![], "imap:test:0:1")],
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(
        envs.len(),
        1,
        "message with no From address must be quarantined, not dropped"
    );
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("dmarc-fail")
    );
}

#[tokio::test]
async fn sender_header_mismatch_rejected() {
    let mut email = make_email("maintainer@example.com", "imap:test:0:1");
    // Sender header claims a different mailbox -- reject.
    email.sender_addr = Some("attacker@example.com".to_string());
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(
        envs.len(),
        1,
        "Sender mismatch must be quarantined, not dropped"
    );
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("off-allowlist")
    );
}

#[tokio::test]
async fn sender_header_matching_maintainer_accepted() {
    let mut email = make_email("maintainer@example.com", "imap:test:0:1");
    email.sender_addr = Some("maintainer@example.com".to_string());
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1, "matching Sender header must be accepted");
}

#[tokio::test]
async fn reply_to_header_is_not_used_for_auth() {
    // Reply-To is irrelevant for authorization; only From (and optionally Sender) matter.
    let mut email = make_email("maintainer@example.com", "imap:test:0:1");
    email
        .headers
        .insert("reply-to".to_string(), "attacker@evil.com".to_string());
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(
        envs.len(),
        1,
        "Reply-To must not affect authorization; only From is checked"
    );
}

// --- Attribution gate: Authentication-Results (ADR-056 Amendment 2026-07-02) ---
//
// These exercise the REAL byte-parsing path (parse_raw_bytes), not hand-built
// RawEmail/authentication_results literals, per the same real-parse discipline
// as poll_preserves_multi_id_wire_references_through_real_parse above: a
// synthetic header map would not exercise mail-parser's actual header
// iteration and could hide a live-path bug the same way it did there.

#[tokio::test]
async fn spoofed_from_no_authentication_results_quarantines_auth_absent() {
    // The original #448 regression: a message whose From: claims to be the
    // maintainer, with no Authentication-Results header at all. Must be
    // quarantined -- and critically, the quarantine envelope's `from` must
    // NOT be the claimed maintainer address, or the vulnerability persists
    // one layer up (comm.ingest stamps from_actor straight from envelope.from).
    let raw = b"From: maintainer@example.com\r\n\
                To: me@example.com\r\n\
                Subject: spoofed, no auth at all\r\n\
                \r\n\
                body";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_ne!(
        envs[0].from, "email:maintainer@example.com",
        "quarantined mail must never carry the claimed maintainer address as `from`"
    );
    assert_eq!(
        envs[0].from, EMAIL_QUARANTINE_SENDER,
        "quarantine sender prefix invariant: `email:` keeps the alert visible to prefix-keyed consumers"
    );
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("auth-absent")
    );
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_claimed_from")
            .map(String::as_str),
        Some("maintainer@example.com"),
        "the claimed From is preserved in metadata for review, but not as `from`"
    );
}

#[tokio::test]
async fn quarantine_claimed_to_preserves_parsed_order_casing_and_json_escaping() {
    let raw = b"From: maintainer@example.com\r\n\
                Delivered-To: delivered@example.com\r\n\
                To: First <FIRST+Tag@Example.COM>, Second <\"A\\\\B\"@Example.NET>, FIRST+Tag@Example.COM\r\n\
                Cc: copy@example.com\r\n\
                In-Reply-To: <untrusted-thread@example.com>\r\n\
                Subject: claimed recipients\r\n\
                \r\n\
                body";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).expect("parsed input");
    let expected = email.to.clone();
    assert_eq!(expected.len(), 3, "{expected:?}");
    // The existing connector lowercases To addresses. Metadata preserves that
    // parsed spelling, order and duplicates, without normalizing a second time.
    assert_eq!(expected[0], "first+tag@example.com");
    assert_eq!(expected[2], expected[0]);
    assert!(expected[1].ends_with("@example.net"), "{expected:?}");
    assert!(
        expected[1].contains('\\') || expected[1].contains('"'),
        "{expected:?}"
    );
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envelopes = ch.poll(Utc::now()).await.expect("quarantine poll");
    assert_eq!(envelopes.len(), 1);
    let envelope = &envelopes[0];
    let encoded = envelope
        .metadata
        .get("quarantine_claimed_to")
        .expect("claimed To metadata");
    let claimed: Vec<String> = serde_json::from_str(encoded).expect("JSON recipient array");
    assert_eq!(claimed, expected);
    assert!(!claimed
        .iter()
        .any(|address| address == "delivered@example.com" || address == "copy@example.com"));
    assert_eq!(envelope.metadata["quarantine_reason"], "auth-absent");
    assert_eq!(envelope.from, EMAIL_QUARANTINE_SENDER);
    assert_eq!(envelope.to, "email:maintainer@example.com");
    assert!(envelope.correlation_external_id.is_none());
    let replay = envelope
        .quarantine_replay
        .as_ref()
        .expect("quarantine replay");
    assert_eq!(replay.notification_to, "email:maintainer@example.com");
    assert_eq!(replay.bytes.as_slice(), raw.as_slice());
}

#[tokio::test]
async fn quarantine_without_to_omits_claimed_recipients_even_with_other_recipient_headers() {
    for headers in [
        "",
        "Delivered-To: delivered@example.com\r\nCc: copy@example.com\r\n",
    ] {
        let raw = format!("From: maintainer@example.com\r\n{headers}Subject: no To\r\n\r\nbody");
        let email =
            parse_raw_bytes(1, raw.as_bytes(), "imap.example.com", 1).expect("parsed input");
        assert!(email.to.is_empty());
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envelopes = ch.poll(Utc::now()).await.expect("quarantine poll");
        assert_eq!(envelopes.len(), 1);
        let envelope = &envelopes[0];
        assert!(!envelope.metadata.contains_key("quarantine_claimed_to"));
        assert_eq!(envelope.from, EMAIL_QUARANTINE_SENDER);
        assert_eq!(envelope.to, "email:maintainer@example.com");
        assert!(envelope.correlation_external_id.is_none());
        let replay = envelope
            .quarantine_replay
            .as_ref()
            .expect("quarantine replay");
        assert_eq!(replay.notification_to, "email:maintainer@example.com");
        assert_eq!(replay.bytes.as_slice(), raw.as_bytes());
    }
}

#[test]
fn malformed_quarantine_omits_claimed_to_even_when_replay_bytes_have_recipients() {
    let ch = build_channel("maintainer@example.com", vec![]);
    let raw = b"From: claimed@example.com\r\nTo: target@example.com\r\n\r\nbody".to_vec();
    for bytes in [None, Some(raw)] {
        for reason in [
            QuarantineReason::MissingBody,
            QuarantineReason::TooLarge,
            QuarantineReason::ParseFailure,
        ] {
            let expected_reason = reason.to_string();
            let envelope = ch.malformed_quarantine_envelope(
                1,
                "imap:imap.example.com:1:1",
                reason,
                bytes.clone(),
            );
            assert!(!envelope.metadata.contains_key("quarantine_claimed_to"));
            assert_eq!(envelope.metadata["quarantine_reason"], expected_reason);
            assert_eq!(envelope.from, EMAIL_QUARANTINE_SENDER);
            assert_eq!(envelope.to, "email:maintainer@example.com");
            assert!(envelope.correlation_external_id.is_none());
            let replay = envelope
                .quarantine_replay
                .as_ref()
                .expect("quarantine replay");
            assert_eq!(replay.notification_to, "email:maintainer@example.com");
            assert_eq!(replay.bytes, bytes.clone().unwrap_or_default());
        }
    }
}

#[tokio::test]
async fn rejected_auth_headers_disclose_only_bounded_untrusted_evidence() {
    for (headers, anchor, reason, family, status, seen_id) in [
        ("", TrustAnchor::AuthservId("mx.example.com".into()), "auth-absent", None, None, None),
        ("Authentication-Results: forged-mx.evil.com; dmarc=pass header.from=example.com\r\n", TrustAnchor::AuthservId("mx.example.com".into()), "auth-untrusted", Some("authentication-results"), Some("present"), Some("forged-mx.evil.com")),
        ("Authentication-Results: mx.microsoft.com 1; spf=pass; dkim=pass header.d=example.com; dmarc=pass header.from=example.com\r\n", TrustAnchor::TopmostNoAuthservId, "auth-untrusted", Some("authentication-results"), Some("present"), Some("mx.microsoft.com")),
        ("ARC-Authentication-Results: i=2; mx.microsoft.com 1; dmarc=pass header.from=example.com\r\n", TrustAnchor::AuthservId("mx.microsoft.com".into()), "auth-untrusted", Some("arc-authentication-results"), Some("present"), Some("mx.microsoft.com")),
        ("Authentication-Results: spf=pass smtp.mailfrom=example.com\r\n", TrustAnchor::AuthservId("mx.example.com".into()), "auth-untrusted", Some("authentication-results"), Some("no-authserv-id"), None),
        ("Authentication-Results:\r\n", TrustAnchor::AuthservId("mx.example.com".into()), "auth-untrusted", Some("authentication-results"), Some("unparseable"), None),
        ("Authentication-Results: other.example.com; dmarc=pass header.from=example.com\r\n", TrustAnchor::TopmostWithAuthservId("mx.example.com".into()), "auth-untrusted", Some("authentication-results"), Some("present"), Some("other.example.com")),
        ("ARC-Authentication-Results: i=bad; mx.microsoft.com\r\n", TrustAnchor::AuthservId("mx.example.com".into()), "auth-untrusted", Some("arc-authentication-results"), Some("unparseable"), None),
    ] {
        let raw = format!("{headers}From: maintainer@example.com\r\nTo: me@example.com\r\nSubject: diagnostic\r\n\r\nbody\r\nARC-Authentication-Results: body text only");
        let email = parse_raw_bytes(1, raw.as_bytes(), "imap.example.com", 1).unwrap();
        let mut config = make_config("maintainer@example.com");
        config.trust_anchor = anchor.clone();
        let ch = build_channel_from(config, vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        let env = &envs[0];
        assert_eq!(env.from, EMAIL_QUARANTINE_SENDER);
        assert_eq!(env.correlation_external_id, None);
        assert!(env.quarantine_replay.is_some());
        assert_eq!(env.metadata.get("quarantine_reason").map(String::as_str), Some(reason));
        assert_eq!(env.metadata.get("quarantine_auth_header_family").map(String::as_str), family);
        assert_eq!(env.metadata.get("quarantine_auth_id_status").map(String::as_str), status);
        assert_eq!(env.metadata.get("quarantine_auth_id").map(String::as_str), seen_id);
        if family.is_some() {
            let mode = match anchor {
                TrustAnchor::AuthservId(_) => "authserv-id",
                TrustAnchor::TopmostNoAuthservId => "topmost-no-authserv-id",
                TrustAnchor::TopmostWithAuthservId(_) => "topmost-with-authserv-id",
            };
            assert_eq!(env.metadata.get("quarantine_auth_anchor_mode").map(String::as_str), Some(mode));
            assert_eq!(env.metadata.get("quarantine_auth_id_truncated").map(String::as_str), Some("false"));
        } else {
            assert!(!env.metadata.keys().any(|key| key.starts_with("quarantine_auth_")));
        }
        assert!(env.metadata.iter().filter(|(key, _)| key.starts_with("quarantine_auth_")).all(|(_, value)| !value.contains("dmarc=pass") && !value.contains("body text")));
    }
}

#[tokio::test]
async fn rejected_auth_header_id_is_bounded_and_store_off_still_drops() {
    let raw = format!("Authentication-Results: {}; dmarc=pass header.from=example.com\r\nFrom: maintainer@example.com\r\nTo: me@example.com\r\n\r\nbody", "x".repeat(300));
    let email = parse_raw_bytes(1, raw.as_bytes(), "imap.example.com", 1).unwrap();
    let ch = build_channel_from(make_config("maintainer@example.com"), vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(
        envs[0].metadata.get("quarantine_auth_id"),
        Some(&"x".repeat(128))
    );
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_auth_id_truncated")
            .map(String::as_str),
        Some("true")
    );

    let email = parse_raw_bytes(1, raw.as_bytes(), "imap.example.com", 1).unwrap();
    let mut config = make_config("maintainer@example.com");
    config.quarantine_store = false;
    let ch = build_channel_from(config, vec![email]);
    assert!(ch.poll(Utc::now()).await.unwrap().is_empty());
}

#[tokio::test]
async fn multi_authentication_results_topmost_trusted_id_wins_over_forged_below() {
    // Two Authentication-Results headers share the trusted authserv-id: the
    // genuine one (dmarc=pass) is topmost (added last, by the final trusted
    // hop); a forged one (dmarc=fail) was injected below it. Topmost wins,
    // per the ADR's trusted-header selection rule -- the genuine stamp is
    // used regardless of what a lower, same-id header claims.
    let raw = b"Authentication-Results: mx.example.com; dmarc=pass header.from=example.com\r\n\
                Authentication-Results: mx.example.com; dmarc=fail header.from=example.com\r\n\
                From: maintainer@example.com\r\n\
                To: me@example.com\r\n\
                Subject: genuine on top, forged below\r\n\
                \r\n\
                body";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].from, "email:maintainer@example.com",
        "topmost genuine dmarc=pass must win over a same-id forged header below it"
    );
}

#[tokio::test]
async fn single_authresults_with_trusted_id_is_mechanically_trusted_operational_precondition() {
    // OPERATIONAL-PRECONDITION BOUNDARY, not a bug: when exactly one
    // Authentication-Results header is present and it carries the
    // configured trusted authserv-id, the gate has no way to distinguish
    // "the receiving MTA genuinely stamped this" from "an attacker injected
    // a single header claiming our trusted id before the (misconfigured or
    // absent) receiving MTA had a chance to strip it and add its own".
    // Per ADR-056 Amendment 2026-07-02 ("Trusted-header selection"), that
    // stripping is a deployment-time responsibility of the receiving
    // boundary, not something this parser re-verifies from message content
    // (doing so would mean deriving trust from the very content the header
    // is supposed to authenticate). Given that precondition holds in
    // production, this header is mechanically trusted.
    let raw = b"Authentication-Results: mx.example.com; dmarc=pass header.from=example.com\r\n\
                From: maintainer@example.com\r\n\
                To: me@example.com\r\n\
                Subject: single AR header, trusted id\r\n\
                \r\n\
                body";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].from, "email:maintainer@example.com");
}

#[tokio::test]
async fn authresults_with_untrusted_authserv_id_is_ignored_quarantines_auth_untrusted() {
    // A header is present, but its authserv-id does not match this
    // deployment's configured trust anchor -- it must be ignored entirely
    // (never treated as a partial signal), leaving no trusted header, hence
    // auth-untrusted.
    let raw = b"Authentication-Results: forged-mx.evil.com; dmarc=pass header.from=example.com\r\n\
                From: maintainer@example.com\r\n\
                To: me@example.com\r\n\
                Subject: wrong authserv-id\r\n\
                \r\n\
                body";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].from, EMAIL_QUARANTINE_SENDER,
        "quarantine sender prefix invariant: `email:` keeps the alert visible to prefix-keyed consumers"
    );
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("auth-untrusted")
    );
}

#[tokio::test]
async fn quoted_reason_semicolon_forging_dmarc_pass_does_not_bypass_gate() {
    // Regression test driven through the REAL byte-parsing
    // path (parse_raw_bytes), not a hand-built AuthResults: a quoted reason=
    // pvalue containing "; dmarc=pass; " must never be split into a separate
    // dmarc method -- the gate must see the genuine spf=fail result and
    // quarantine, never attribute to the maintainer.
    let raw = b"Authentication-Results: mx.example.com; spf=fail reason=\"remote said; dmarc=pass; still fail\" smtp.mailfrom=attacker.net\r\n\
                From: maintainer@example.com\r\n\
                To: me@example.com\r\n\
                Subject: quoted reason attack\r\n\
                \r\n\
                body";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].from, EMAIL_QUARANTINE_SENDER,
        "quarantine sender prefix invariant: `email:` keeps the alert visible to prefix-keyed consumers"
    );
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("dmarc-fail")
    );
}

#[tokio::test]
async fn quoted_whitespace_smtp_mailfrom_injection_does_not_bypass_gate() {
    // Regression for the #501 property-tokenizer hardening, driven through
    // the REAL byte-parsing path (parse_raw_bytes): a quoted
    // smtp.mailfrom= pvalue containing embedded whitespace and an
    // attacker-controlled inner "smtp.mailfrom=example.com" must never be
    // shattered into a separate, alignment-winning property token. The
    // real envelope domain (evil.com) must be what the gate sees, so this
    // must quarantine as unaligned, never attribute to the maintainer.
    let raw = b"Authentication-Results: mx.example.com; spf=pass smtp.mailfrom=\"attacker smtp.mailfrom=example.com \"@evil.com\r\n\
                From: maintainer@example.com\r\n\
                To: me@example.com\r\n\
                Subject: quoted whitespace property injection\r\n\
                \r\n\
                body";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].from, EMAIL_QUARANTINE_SENDER,
        "quarantine sender prefix invariant: `email:` keeps the alert visible to prefix-keyed consumers"
    );
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("unaligned")
    );
}

#[tokio::test]
async fn spf_pass_unaligned_envelope_from_quarantines_unaligned() {
    let raw =
        b"Authentication-Results: mx.example.com; spf=pass smtp.mailfrom=alice@attacker.net\r\n\
                From: maintainer@example.com\r\n\
                To: me@example.com\r\n\
                Subject: spf pass, mismatched envelope domain\r\n\
                \r\n\
                body";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].from, EMAIL_QUARANTINE_SENDER,
        "quarantine sender prefix invariant: `email:` keeps the alert visible to prefix-keyed consumers"
    );
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("unaligned")
    );
}

#[tokio::test]
async fn dkim_pass_aligned_d_domain_is_attributed() {
    let raw =
        b"Authentication-Results: mx.example.com; dkim=pass header.d=example.com header.s=sel1\r\n\
                From: maintainer@example.com\r\n\
                To: me@example.com\r\n\
                Subject: dkim pass, aligned d=\r\n\
                \r\n\
                body";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
    let ch = build_channel("maintainer@example.com", vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].from, "email:maintainer@example.com");
}

// --- EXO no-authserv-id trust anchor, end-to-end (ADR-056 Amendment 2026-07-03) ---

#[tokio::test]
async fn exo_no_authserv_id_fixture_end_to_end_is_attributed() {
    // Real Exchange Online header shape (verbatim values from the live
    // fixture `raw_headers.txt`): an ARC-Authentication-Results
    // header (must be ignored -- collecting/trusting ARC is the rejected
    // Shape B, out of scope) precedes the plain Authentication-Results
    // header EXO stamps on its own internal hop, which carries no
    // authserv-id at all. In TopmostNoAuthservId mode this must attribute
    // cleanly through the real byte-parsing path.
    let raw = b"ARC-Authentication-Results: i=2; mx.microsoft.com 1; spf=pass smtp.mailfrom=gmail.com; dmarc=pass header.from=gmail.com; dkim=pass header.d=gmail.com\r\n\
                Authentication-Results: spf=pass (sender IP is 2607:f8b0:4864:20::1129) smtp.mailfrom=gmail.com; dkim=pass (signature was verified) header.d=gmail.com;dmarc=pass action=none header.from=gmail.com;compauth=pass reason=100\r\n\
                From: Example Sender <sender@gmail.com>\r\n\
                To: Example Recipient <recipient@example.com>\r\n\
                Subject: Khive email subject disappears into body\r\n\
                \r\n\
                body text";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();

    let mut config = make_config("sender@gmail.com");
    config.trust_anchor = TrustAnchor::TopmostNoAuthservId;
    let ch = build_channel_from(config, vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].from, "email:sender@gmail.com",
        "the real EXO no-authserv-id header must attribute cleanly in TopmostNoAuthservId mode"
    );
}

#[tokio::test]
async fn exo_mode_topmost_carrying_an_authserv_id_quarantines_auth_untrusted() {
    // If the topmost Authentication-Results unexpectedly carries an
    // authserv-id while this deployment is configured for
    // TopmostNoAuthservId mode, that violates the invariant that EXO's
    // own stamp is topmost and unadorned (Microsoft could start emitting
    // one, or an attacker's forged header floated to the top) --
    // quarantine rather than trust it.
    let raw = b"Authentication-Results: mx.microsoft.com; dmarc=pass header.from=gmail.com\r\n\
                From: Example Sender <sender@gmail.com>\r\n\
                To: Example Recipient <recipient@example.com>\r\n\
                Subject: forged topmost carries an id\r\n\
                \r\n\
                body";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
    let mut config = make_config("sender@gmail.com");
    config.trust_anchor = TrustAnchor::TopmostNoAuthservId;
    let ch = build_channel_from(config, vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0].from, EMAIL_QUARANTINE_SENDER,
        "quarantine sender prefix invariant: `email:` keeps the alert visible to prefix-keyed consumers"
    );
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("auth-untrusted")
    );
}

// --- Topmost trust with a shared authserv-id, end-to-end (ADR-056 Amendment 2026-10-15) ---

#[tokio::test]
async fn exo_stamp_with_shared_authserv_id_end_to_end_is_attributed_in_topmost_mode() {
    // Exchange Online stamps `mx.microsoft.com 1` on its own topmost plain
    // header for mail from some external senders (shape from a delivered
    // message; addresses and domains replaced with example values). No ARC
    // header is present. The no-authserv-id mode refuses it; the topmost
    // mode configured with that id attributes it.
    let raw = b"Authentication-Results: mx.microsoft.com 1; spf=pass (sender IP is 192.0.2.10) smtp.mailfrom=send.example.com; dkim=pass (signature was verified) header.d=example.com;dkim=pass (signature was verified) header.d=amazonses.com;dmarc=pass action=none header.from=example.com;compauth=pass reason=100\r\n\
                Received-SPF: Pass (protection.outlook.com: domain of send.example.com designates 192.0.2.10 as permitted sender) receiver=protection.outlook.com; client-ip=192.0.2.10; helo=mail.example.net; pr=C\r\n\
                From: Example Sender <noreply@example.com>\r\n\
                To: Example Recipient <recipient@example.org>\r\n\
                Subject: sign-in link\r\n\
                \r\n\
                body text";

    for (anchor, attributed) in [
        (TrustAnchor::TopmostNoAuthservId, false),
        (
            TrustAnchor::TopmostWithAuthservId("mx.microsoft.com".to_string()),
            true,
        ),
    ] {
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        let mut config = make_config("noreply@example.com");
        config.trust_anchor = anchor.clone();
        let ch = build_channel_from(config, vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        if attributed {
            assert_eq!(envs[0].from, "email:noreply@example.com", "{anchor:?}");
        } else {
            assert_eq!(envs[0].from, EMAIL_QUARANTINE_SENDER, "{anchor:?}");
            assert_eq!(
                envs[0]
                    .metadata
                    .get("quarantine_reason")
                    .map(String::as_str),
                Some("auth-untrusted"),
                "{anchor:?}"
            );
        }
    }
}

#[tokio::test]
async fn exo_unstamped_gmail_shape_still_attributes_in_topmost_with_authserv_id_mode() {
    // Gmail-origin mail to the same mailbox carries no authserv-id on the
    // topmost plain header, and `mx.microsoft.com 1` only on ARC, which is
    // never trusted. The topmost mode accepts the unstamped shape too.
    let raw = b"ARC-Authentication-Results: i=2; mx.microsoft.com 1; spf=pass smtp.mailfrom=gmail.com; dmarc=pass header.from=gmail.com; dkim=pass header.d=gmail.com\r\n\
                Authentication-Results: spf=pass (sender IP is 2607:f8b0:4864:20::1129) smtp.mailfrom=gmail.com; dkim=pass (signature was verified) header.d=gmail.com;dmarc=pass action=none header.from=gmail.com;compauth=pass reason=100\r\n\
                From: Example Sender <sender@gmail.com>\r\n\
                To: Example Recipient <recipient@example.com>\r\n\
                Subject: unstamped topmost\r\n\
                \r\n\
                body text";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
    let mut config = make_config("sender@gmail.com");
    config.trust_anchor = TrustAnchor::TopmostWithAuthservId("mx.microsoft.com".to_string());
    let ch = build_channel_from(config, vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].from, "email:sender@gmail.com");
}

#[tokio::test]
async fn topmost_with_authserv_id_mode_ignores_a_forged_pass_below_a_failing_stamp() {
    // The boundary's genuine failing stamp is topmost; a sender-forged
    // passing header with the same shared id sits below it.
    let raw = b"Authentication-Results: mx.microsoft.com 1; spf=fail smtp.mailfrom=evil.example; dkim=none; dmarc=fail action=quarantine header.from=example.com;compauth=fail reason=000\r\n\
                Authentication-Results: mx.microsoft.com 1; spf=pass smtp.mailfrom=example.com; dkim=pass header.d=example.com; dmarc=pass action=none header.from=example.com\r\n\
                From: maintainer@example.com\r\n\
                To: me@example.com\r\n\
                Subject: forged pass below the stamp\r\n\
                \r\n\
                body";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
    let mut config = make_config("maintainer@example.com");
    config.trust_anchor = TrustAnchor::TopmostWithAuthservId("mx.microsoft.com".to_string());
    let ch = build_channel_from(config, vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].from, EMAIL_QUARANTINE_SENDER);
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("dmarc-fail"),
        "the topmost stamp is the one trusted, and it fails"
    );
}

#[tokio::test]
async fn quarantine_store_off_drops_unattributed_message() {
    let mut config = make_config("maintainer@example.com");
    config.quarantine_store = false;
    let raw = b"From: maintainer@example.com\r\n\
                To: me@example.com\r\n\
                Subject: no auth, quarantine store off\r\n\
                \r\n\
                body";
    let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
    let ch = build_channel_from(config, vec![email]);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert!(
        envs.is_empty(),
        "quarantine_store=false must drop the message, not store it"
    );
}

// --- Batch isolation (Fix 3) ---

#[tokio::test]
async fn bad_message_in_batch_does_not_abort_poll() {
    let ch = build_channel(
        "maintainer@example.com",
        vec![
            // First message: unauthorized -- must be quarantined, not abort the batch.
            make_email("attacker@example.com", "imap:test:0:1"),
            // Second message: authorized -- must be attributed.
            make_email("maintainer@example.com", "imap:test:0:2"),
        ],
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 2, "both messages must produce an envelope");

    let quarantined = envs
        .iter()
        .find(|e| e.external_id.as_deref() == Some("imap:test:user@example.com:0:1"))
        .expect("unauthorized message must still be present, quarantined");
    assert_eq!(
        quarantined.from, EMAIL_QUARANTINE_SENDER,
        "quarantine sender prefix invariant: `email:` keeps the alert visible to prefix-keyed consumers"
    );
    assert_eq!(
        quarantined
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("off-allowlist")
    );

    let attributed = envs
        .iter()
        .find(|e| e.external_id.as_deref() == Some("imap:test:user@example.com:0:2"))
        .expect("authorized message must be attributed");
    assert_eq!(attributed.from, "email:maintainer@example.com");
}

// --- SMTP send path ---

#[tokio::test]
async fn send_strips_email_prefix() {
    let config = make_config("maintainer@example.com");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let smtp = SmtpSender::with_connector(RecordingSmtp {
        calls: calls.clone(),
    });
    let imap = ImapFetcher::with_connector(FixedImap { emails: vec![] });
    let ch = EmailChannel::with_connectors(config, smtp, imap);

    let env = ChannelEnvelope::new("email:from@example.com", "email:to@example.com", "hello");
    ch.send(env).await.unwrap();

    let recorded = calls.lock().unwrap();
    assert_eq!(recorded[0], "from@example.com->to@example.com");
}

#[tokio::test]
async fn send_forwards_in_reply_to_to_connector() {
    struct CapturingSmtp {
        captured: Arc<Mutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl SmtpConnector for CapturingSmtp {
        async fn deliver(
            &self,
            _from: &str,
            _to: &str,
            _subject: &str,
            _body: &str,
            _tid: Option<&str>,
            _message_id: Option<&str>,
            in_reply_to: Option<&str>,
            _references: Option<&str>,
            _list_unsubscribe: Option<&str>,
        ) -> Result<(), ChannelError> {
            self.captured
                .lock()
                .unwrap()
                .push(in_reply_to.map(|s| s.to_string()));
            Ok(())
        }
    }

    let config = make_config("maintainer@example.com");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let smtp = SmtpSender::with_connector(CapturingSmtp {
        captured: captured.clone(),
    });
    let imap = ImapFetcher::with_connector(FixedImap { emails: vec![] });
    let ch = EmailChannel::with_connectors(config, smtp, imap);

    let env = ChannelEnvelope::new("email:from@example.com", "email:to@example.com", "hello")
        .with_in_reply_to("<parent123@example.com>");
    ch.send(env).await.unwrap();

    let vals = captured.lock().unwrap();
    assert_eq!(vals[0].as_deref(), Some("<parent123@example.com>"));
}

#[tokio::test]
async fn send_forwards_references_to_connector() {
    struct CapturingSmtp {
        captured: Arc<Mutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl SmtpConnector for CapturingSmtp {
        async fn deliver(
            &self,
            _from: &str,
            _to: &str,
            _subject: &str,
            _body: &str,
            _tid: Option<&str>,
            _message_id: Option<&str>,
            _in_reply_to: Option<&str>,
            references: Option<&str>,
            _list_unsubscribe: Option<&str>,
        ) -> Result<(), ChannelError> {
            self.captured
                .lock()
                .unwrap()
                .push(references.map(|s| s.to_string()));
            Ok(())
        }
    }

    let config = make_config("maintainer@example.com");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let smtp = SmtpSender::with_connector(CapturingSmtp {
        captured: captured.clone(),
    });
    let imap = ImapFetcher::with_connector(FixedImap { emails: vec![] });
    let ch = EmailChannel::with_connectors(config, smtp, imap);

    let env = ChannelEnvelope::new("email:from@example.com", "email:to@example.com", "hello")
        .with_in_reply_to("<parent123@example.com>")
        .with_references("<grandparent1@example.com> <parent123@example.com>");
    ch.send(env).await.unwrap();

    let vals = captured.lock().unwrap();
    assert_eq!(
        vals[0].as_deref(),
        Some("<grandparent1@example.com> <parent123@example.com>")
    );
}

// --- checkpoint decode (issue #449) ---

#[test]
fn source_mismatch_drops_stored_progress() {
    let ch = build_channel_from(make_config("maintainer@example.com"), vec![]);
    let stored = StoredChannelCheckpoint {
        checkpoint: ChannelCheckpoint {
            source: "imap+tls:other.example.com:993:other@example.com:INBOX".to_string(),
            generation: 42,
            high_water: Some(10),
        },
        committed_at: Utc::now(),
    };
    let (progress, committed_at) = ch.decode_progress(Some(&stored)).unwrap();
    assert_eq!(
        progress,
        ImapProgress::default(),
        "a source mismatch (different host/port/mailbox) must drop stored progress"
    );
    assert!(
        committed_at.is_none(),
        "a source mismatch must not surface the other source's recovery floor either"
    );
}

#[test]
fn invalid_persisted_imap_width_or_zero_fails_closed() {
    let ch = build_channel_from(make_config("maintainer@example.com"), vec![]);
    let source = ch.checkpoint_source();

    let overflowing_generation = StoredChannelCheckpoint {
        checkpoint: ChannelCheckpoint {
            source: source.clone(),
            generation: u64::from(u32::MAX) + 1,
            high_water: None,
        },
        committed_at: Utc::now(),
    };
    assert!(
        ch.decode_progress(Some(&overflowing_generation)).is_err(),
        "a generation wider than u32 must fail closed, not truncate"
    );

    let zero_generation = StoredChannelCheckpoint {
        checkpoint: ChannelCheckpoint {
            source: source.clone(),
            generation: 0,
            high_water: None,
        },
        committed_at: Utc::now(),
    };
    assert!(
        ch.decode_progress(Some(&zero_generation)).is_err(),
        "a zero generation must fail closed, not silently reset to empty progress"
    );

    let overflowing_high_water = StoredChannelCheckpoint {
        checkpoint: ChannelCheckpoint {
            source,
            generation: 1,
            high_water: Some(u64::from(u32::MAX) + 1),
        },
        committed_at: Utc::now(),
    };
    assert!(
        ch.decode_progress(Some(&overflowing_high_water)).is_err(),
        "a high_water wider than u32 must fail closed"
    );
}

#[test]
fn strip_kind_prefix_removes_prefix() {
    assert_eq!(
        strip_kind_prefix("email:user@example.com", "email"),
        "user@example.com"
    );
    assert_eq!(
        strip_kind_prefix("user@example.com", "email"),
        "user@example.com"
    );
}

// --- poll_page / poll integration (issue #449) ---
//
// `select_uid_page`/`next_progress` are module-private to `connector::imap`
// and covered directly by that module's own tests. The mock below
// replicates the same sort/dedup/high-water-filter/truncate contract
// inline so these tests can drive the actual `EmailChannel::poll_page`/
// `poll` orchestration (disposition, checkpoint encode/decode, source
// binding), not just the connector-level helpers in isolation.

struct KeysetMockImap {
    validity: NonZeroU32,
    all_uids: Vec<u32>,
}

impl KeysetMockImap {
    fn select(&self, limit: usize, progress: ImapProgress) -> Vec<NonZeroU32> {
        let mut uids = self.all_uids.clone();
        uids.sort_unstable();
        uids.dedup();
        if progress.uid_validity == Some(self.validity) {
            if let Some(high_water) = progress.last_seen_uid {
                uids.retain(|&u| u > high_water.get());
            }
        }
        uids.truncate(limit);
        uids.into_iter()
            .map(|u| NonZeroU32::new(u).unwrap())
            .collect()
    }

    fn email_for(&self, uid: NonZeroU32) -> RawEmail {
        let mut email = make_email(
            "sender@example.com",
            &format!(
                "imap:mail.example.com:{}:{}",
                self.validity.get(),
                uid.get()
            ),
        );
        email.uid = uid.get();
        email
    }
}

#[async_trait]
impl ImapConnector for KeysetMockImap {
    async fn fetch_page(
        &self,
        _since: DateTime<Utc>,
        limit: usize,
        progress: ImapProgress,
    ) -> Result<ImapFetchPage, ChannelError> {
        let selected = self.select(limit, progress);
        let emails: Vec<SelectedMessage> = selected
            .iter()
            .map(|&u| SelectedMessage::Email(Box::new(self.email_for(u))))
            .collect();
        let next_progress = match selected.iter().map(|u| u.get()).max() {
            Some(max) => ImapProgress {
                uid_validity: Some(self.validity),
                last_seen_uid: NonZeroU32::new(max),
            },
            None if progress.uid_validity == Some(self.validity) => progress,
            None => ImapProgress {
                uid_validity: Some(self.validity),
                last_seen_uid: None,
            },
        };
        Ok(ImapFetchPage {
            emails,
            next_progress,
        })
    }
}

fn build_keyset_channel(mailbox: &str, validity: u32, all_uids: Vec<u32>) -> EmailChannel {
    let mut config = make_config("maintainer@example.com");
    config.mailbox = mailbox.to_string();
    config.username = mailbox.to_string();
    let smtp = SmtpSender::with_connector(RecordingSmtp {
        calls: Arc::new(Mutex::new(Vec::new())),
    });
    let imap = ImapFetcher::with_connector(KeysetMockImap {
        validity: NonZeroU32::new(validity).unwrap(),
        all_uids,
    });
    EmailChannel::with_connectors(config, smtp, imap)
}

#[tokio::test]
async fn poll_page_resumes_from_persisted_checkpoint_across_restart() {
    let since = Utc::now();

    // First "process": drains the initial backlog 1..=5 and produces a
    // checkpoint candidate.
    let first = build_keyset_channel("a@example.com", 7, vec![1, 2, 3, 4, 5]);
    let page1 = first.poll_page(since, None).await.unwrap();
    assert_eq!(page1.envelopes.len(), 5);
    let checkpoint = page1
        .next_checkpoint
        .expect("non-empty page must checkpoint");
    let stored = StoredChannelCheckpoint {
        checkpoint,
        committed_at: Utc::now(),
    };

    // A brand-new `EmailChannel` instance (simulating a process restart:
    // no shared in-memory state) sees the same 1..=5 backlog plus one
    // newly-arrived UID 6.
    let restarted = build_keyset_channel("a@example.com", 7, vec![1, 2, 3, 4, 5, 6]);
    let page2 = restarted.poll_page(since, Some(&stored)).await.unwrap();
    assert_eq!(
        page2.envelopes.len(),
        1,
        "a fresh instance given the persisted checkpoint must fetch zero \
         already-seen messages"
    );
    assert_eq!(
        page2.envelopes[0].external_id.as_deref(),
        Some("imap:mail.example.com:a@example.com:7:6"),
        "the fresh instance must correctly pick up the newly-arrived UID"
    );
    assert_eq!(
        page2.next_checkpoint.unwrap().high_water,
        Some(6),
        "checkpoint must advance to the newly-arrived UID"
    );
}

#[tokio::test]
async fn poll_page_two_mailboxes_have_independent_cursors() {
    let since = Utc::now();
    let mailbox_a_checkpoint = StoredChannelCheckpoint {
        checkpoint: ChannelCheckpoint {
            source: "imap+tls:imap.example.com:993:a@example.com:INBOX".to_string(),
            generation: 99,
            high_water: Some(50),
        },
        committed_at: Utc::now(),
    };

    // Mailbox B has its own small, low-numbered backlog untouched by A's
    // checkpoint.
    let mailbox_b = build_keyset_channel("b@example.com", 5, vec![1, 2, 3]);
    let page = mailbox_b
        .poll_page(since, Some(&mailbox_a_checkpoint))
        .await
        .unwrap();

    assert_eq!(
        page.envelopes.len(),
        3,
        "mailbox A's checkpoint must be rejected by B's source check, leaving \
         B's own backlog untouched"
    );
    assert_eq!(page.next_checkpoint.unwrap().generation, 5);
}

#[tokio::test]
async fn poll_page_retried_call_with_uncommitted_checkpoint_is_idempotent() {
    let since = Utc::now();
    let ch = build_keyset_channel("a@example.com", 9, vec![1, 2, 3]);

    let first = ch.poll_page(since, None).await.unwrap();
    // Simulate a retry after a crash between fetch and cursor commit: the
    // coordinator calls poll_page again with the same (uncommitted) `None`.
    let retry = ch.poll_page(since, None).await.unwrap();

    let ids = |p: &ChannelPollPage| {
        p.envelopes
            .iter()
            .map(|e| e.external_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&first),
        ids(&retry),
        "retrying with the same uncommitted checkpoint must return \
         byte-identical envelopes"
    );
    assert_eq!(
        first.next_checkpoint.unwrap(),
        retry.next_checkpoint.unwrap(),
        "retrying with the same uncommitted checkpoint must return an \
         identical checkpoint candidate"
    );
}

#[tokio::test]
async fn poll_page_quarantine_disabled_drop_still_advances_checkpoint() {
    let since = Utc::now();
    let mut config = make_config("maintainer@example.com");
    config.quarantine_store = false;
    let smtp = SmtpSender::with_connector(RecordingSmtp {
        calls: Arc::new(Mutex::new(Vec::new())),
    });

    // A message with no Authentication-Results header fails the gate
    // (AuthAbsent) and, with quarantine_store disabled, is dropped rather
    // than emitted as an envelope.
    let mut gate_failing = make_email("forged@example.com", "imap:mail.example.com:3:1");
    gate_failing.authentication_results.clear();
    gate_failing.uid = 1;

    struct SingleGateFailingImap {
        email: RawEmail,
    }
    #[async_trait]
    impl ImapConnector for SingleGateFailingImap {
        async fn fetch_page(
            &self,
            _since: DateTime<Utc>,
            _limit: usize,
            _progress: ImapProgress,
        ) -> Result<ImapFetchPage, ChannelError> {
            Ok(ImapFetchPage {
                emails: vec![SelectedMessage::Email(Box::new(self.email.clone()))],
                next_progress: ImapProgress {
                    uid_validity: NonZeroU32::new(3),
                    last_seen_uid: NonZeroU32::new(self.email.uid),
                },
            })
        }
    }
    let imap = ImapFetcher::with_connector(SingleGateFailingImap {
        email: gate_failing,
    });
    let ch = EmailChannel::with_connectors(config, smtp, imap);

    let page = ch.poll_page(since, None).await.unwrap();
    assert!(
        page.envelopes.is_empty(),
        "a gate-failing message with quarantine_store disabled must be dropped, \
         not emitted"
    );
    assert_eq!(
        page.next_checkpoint.unwrap().high_water,
        Some(1),
        "the checkpoint must still advance past the dropped message's UID -- \
         checkpoint advancement is bound to the selected UID page, not to \
         which envelopes disposition actually emits"
    );
}

/// khive #449 follow-up: proves a `Malformed` disposition survives
/// end-to-end through `poll_page` into the emitted `ChannelEnvelope`,
/// not just `process_selected_page`'s intermediate value.
#[tokio::test]
async fn poll_page_malformed_uid_produces_a_stable_external_id_and_quarantine_metadata() {
    let since = Utc::now();
    let config = make_config("maintainer@example.com");
    let smtp = SmtpSender::with_connector(RecordingSmtp {
        calls: Arc::new(Mutex::new(Vec::new())),
    });

    struct PoisonUidImap {
        good: RawEmail,
    }
    #[async_trait]
    impl ImapConnector for PoisonUidImap {
        async fn fetch_page(
            &self,
            _since: DateTime<Utc>,
            _limit: usize,
            _progress: ImapProgress,
        ) -> Result<ImapFetchPage, ChannelError> {
            Ok(ImapFetchPage {
                emails: vec![
                    SelectedMessage::Malformed {
                        uid: 1,
                        imap_external_id: "imap:imap.example.com:4:1".to_string(),
                        reason: MalformedReason::MissingBody,
                        raw_bytes: None,
                    },
                    SelectedMessage::Email(Box::new(self.good.clone())),
                ],
                next_progress: ImapProgress {
                    uid_validity: NonZeroU32::new(4),
                    last_seen_uid: NonZeroU32::new(2),
                },
            })
        }
    }

    let mut good = make_email("sender@example.com", "imap:imap.example.com:4:2");
    good.uid = 2;
    let imap = ImapFetcher::with_connector(PoisonUidImap { good });
    let ch = EmailChannel::with_connectors(config, smtp, imap);

    let page = ch.poll_page(since, None).await.unwrap();
    assert_eq!(
        page.envelopes.len(),
        2,
        "the poison UID must still produce a durable quarantine envelope \
         alongside the valid message, not a dropped/short page"
    );

    let quarantined = page
        .envelopes
        .iter()
        .find(|e| e.external_id.as_deref() == Some("imap:imap.example.com:user@example.com:4:1"))
        .expect(
            "the malformed UID's stable external_id must be present on the \
             envelope actually handed to comm.ingest",
        );
    assert_eq!(
        quarantined.from, EMAIL_QUARANTINE_SENDER,
        "quarantine sender prefix invariant: `email:` keeps the alert visible to prefix-keyed consumers"
    );
    let replay = quarantined
        .quarantine_replay
        .as_ref()
        .expect("even a missing IMAP body carries an explicit empty replay context");
    assert!(replay.bytes.is_empty());
    assert_eq!(replay.notification_to, "email:maintainer@example.com");
    assert_eq!(
        quarantined.metadata.get("quarantined").map(String::as_str),
        Some("true"),
        "the envelope must carry durable quarantine metadata for comm.ingest \
         to persist"
    );
    assert_eq!(
        quarantined
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("missing-body"),
        "the quarantine reason must be preserved onto the persisted envelope"
    );

    assert_eq!(
        page.next_checkpoint.unwrap().high_water,
        Some(2),
        "the checkpoint candidate must advance past the poison UID"
    );
}

#[tokio::test]
async fn oversized_uid_is_quarantined_without_body_even_if_attribution_quarantine_is_off() {
    let mut config = make_config("maintainer@example.com");
    config.quarantine_store = false;
    let ch = build_channel_from(config, vec![]);
    let mut good = make_email("maintainer@example.com", "imap:imap.example.com:4:2");
    good.uid = 2;
    let (envelopes, _) = ch
        .disposition(
            vec![
                SelectedMessage::Malformed {
                    uid: 1,
                    imap_external_id: "imap:imap.example.com:4:1".to_string(),
                    reason: MalformedReason::TooLarge,
                    raw_bytes: None,
                },
                SelectedMessage::Email(Box::new(good)),
            ],
            None,
        )
        .await
        .unwrap();

    assert_eq!(envelopes.len(), 2);
    assert_eq!(envelopes[0].from, EMAIL_QUARANTINE_SENDER);
    assert_eq!(
        envelopes[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("too-large")
    );
    assert!(envelopes[0]
        .quarantine_replay
        .as_ref()
        .unwrap()
        .bytes
        .is_empty());
    assert_eq!(
        envelopes[1].external_id.as_deref(),
        Some("imap:imap.example.com:user@example.com:4:2")
    );
}

#[tokio::test]
async fn poll_drains_75_same_day_backlog_across_repeated_calls_via_production_entrypoint() {
    // Drives the actual production entrypoint (`EmailChannel::poll`, the
    // literal call `serve.rs` makes) with the issue's own numbers: 75
    // same-day UIDs, page limit 50, shuffled input mirroring HashSet
    // iteration-order noise.
    let mut all_uids: Vec<u32> = (1..=75).rev().collect();
    all_uids.extend(1..=10); // duplicates
    let ch = build_keyset_channel("a@example.com", 42, all_uids);

    let since = Utc::now();
    let page1 = ch.poll(since).await.unwrap();
    assert_eq!(page1.len(), 50, "first poll must return exactly 50");
    let page2 = ch.poll(since).await.unwrap();
    assert_eq!(
        page2.len(),
        25,
        "second poll must return the remaining 25, not a repeat of the first 50"
    );
    let page3 = ch.poll(since).await.unwrap();
    assert!(
        page3.is_empty(),
        "third poll must be empty; backlog fully drained"
    );

    let mut all_ids: Vec<String> = page1
        .iter()
        .chain(page2.iter())
        .map(|e| e.external_id.clone().unwrap())
        .collect();
    all_ids.sort();
    all_ids.dedup();
    assert_eq!(
        all_ids.len(),
        75,
        "all 75 external IDs must be distinct across both calls -- none skipped, \
         none repeated"
    );
}

// Use the builders themselves, then pass the same fields as serve.rs's
// channel poll loop. The replay bytes are handled separately by the loop.
fn quarantine_ingest_cases() -> Vec<(&'static str, ChannelEnvelope)> {
    let ch = build_channel("maintainer@example.com", vec![]);
    let gate_email = make_email("outsider@example.com", "imap:imap.example.com:17:41");
    vec![
        (
            "attribution gate",
            ch.quarantine_envelope(&gate_email, QuarantineReason::OffAllowlist),
        ),
        (
            "malformed UID",
            ch.malformed_quarantine_envelope(
                42,
                "imap:imap.example.com:17:42",
                QuarantineReason::MissingBody,
                None,
            ),
        ),
    ]
}

fn quarantine_ingest_params(env: &ChannelEnvelope, slug: &str) -> serde_json::Value {
    serde_json::json!({
        "namespace": "local",
        "from": env.from.clone(),
        "to": env.to.clone(),
        "content": env.content.clone(),
        "subject": env.subject.clone(),
        "channel_kind": "email",
        "channel_slug": slug,
        "external_id": env.external_id.clone(),
        "legacy_external_id": env.legacy_external_id.clone(),
        "sent_at": env.sent_at.as_ref().map(|ts| ts.to_rfc3339()),
        "correlation_external_id": env.correlation_external_id.clone(),
        "default_inbound_actor": "lambda:maintainer",
        "wire_message_id": env.wire_message_id.clone(),
        "wire_references": env.wire_references.clone(),
        "metadata": env.metadata.clone(),
    })
}

fn quarantine_ingest_registry() -> (khive_runtime::VerbRegistry, khive_runtime::KhiveRuntime) {
    use khive_pack_comm as _;
    use khive_pack_kg as _;
    use khive_runtime::{KhiveRuntime, PackRegistry, VerbRegistryBuilder};

    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(
        &["kg".to_string(), "comm".to_string()],
        runtime.clone(),
        &mut builder,
    )
    .expect("register packs");
    builder.with_default_namespace("local");
    (builder.build().expect("build registry"), runtime)
}

#[tokio::test]
async fn both_email_quarantine_builders_ingest_on_first_call() {
    use khive_runtime::Namespace;

    for (builder, env) in quarantine_ingest_cases() {
        let (registry, runtime) = quarantine_ingest_registry();
        let params = quarantine_ingest_params(&env, "user@example.com");
        let reason = env
            .metadata
            .get("quarantine_reason")
            .expect("adapter reason");
        assert!(params["legacy_external_id"].is_string(), "{builder}");
        assert_eq!(params["metadata"]["quarantined"], "true", "{builder}");
        let claimed_to = if builder == "attribution gate" {
            Some("[\"me@example.com\"]")
        } else {
            None
        };
        assert_eq!(
            params["metadata"]["quarantine_claimed_to"].as_str(),
            claimed_to,
            "{builder}"
        );

        let first = registry
            .dispatch("comm.ingest", params.clone())
            .await
            .unwrap_or_else(|error| panic!("{builder} first ingest: {error}"));
        assert_eq!(first["deduplicated"], false, "{builder}");
        let retry = registry
            .dispatch("comm.ingest", params)
            .await
            .unwrap_or_else(|error| panic!("{builder} retry: {error}"));
        assert_eq!(retry["deduplicated"], true, "{builder}");
        assert_eq!(retry["thread_id"], first["thread_id"], "{builder}");

        let token = runtime.authorize(Namespace::local()).expect("local token");
        let notes = runtime.notes(&token).expect("note store");
        assert_eq!(
            notes.count_notes("local", Some("message")).await.unwrap(),
            1,
            "{builder}"
        );
        let id = first["full_id"]
            .as_str()
            .expect("note id")
            .parse()
            .expect("UUID");
        let note = notes.get_note(id).await.unwrap().expect("quarantine note");
        let props = note.properties.expect("message properties");
        assert_eq!(
            props["external_id"],
            env.external_id.as_deref().unwrap(),
            "{builder}"
        );
        assert_eq!(
            props["quarantine_reason"].as_str(),
            Some(reason.as_str()),
            "{builder}"
        );
        assert_eq!(props["quarantined"], "true", "{builder}");
        assert_eq!(props["from_actor"], EMAIL_QUARANTINE_SENDER, "{builder}");
        assert_eq!(props["to"], "email:maintainer@example.com", "{builder}");
        assert_eq!(props["to_actor"], "lambda:maintainer", "{builder}");
        assert_eq!(
            props
                .get("quarantine_claimed_to")
                .and_then(serde_json::Value::as_str),
            claimed_to,
            "{builder}"
        );
    }
}

#[tokio::test]
async fn both_email_quarantine_builders_dedup_old_key_without_rewriting_it() {
    use khive_runtime::Namespace;

    for (builder, env) in quarantine_ingest_cases() {
        let (registry, runtime) = quarantine_ingest_registry();
        let mut old_params = quarantine_ingest_params(&env, "user@example.com");
        let old_id = env.legacy_external_id.as_deref().expect("old IMAP key");
        old_params["external_id"] = serde_json::json!(old_id);
        old_params
            .as_object_mut()
            .unwrap()
            .remove("legacy_external_id");
        let old = registry
            .dispatch("comm.ingest", old_params)
            .await
            .unwrap_or_else(|error| panic!("{builder} old-key seed: {error}"));
        assert_eq!(old["deduplicated"], false, "{builder}");

        let replay = registry
            .dispatch(
                "comm.ingest",
                quarantine_ingest_params(&env, "user@example.com"),
            )
            .await
            .unwrap_or_else(|error| panic!("{builder} old-key replay: {error}"));
        assert_eq!(replay["deduplicated"], true, "{builder}");
        assert_eq!(replay["thread_id"], old["thread_id"], "{builder}");

        let token = runtime.authorize(Namespace::local()).expect("local token");
        let notes = runtime.notes(&token).expect("note store");
        assert_eq!(
            notes.count_notes("local", Some("message")).await.unwrap(),
            1,
            "{builder}"
        );
        let id = old["full_id"]
            .as_str()
            .expect("note id")
            .parse()
            .expect("UUID");
        let note = notes
            .get_note(id)
            .await
            .unwrap()
            .expect("old quarantine note");
        let props = note.properties.expect("message properties");
        assert_eq!(props["external_id"], old_id, "{builder}");
        assert_eq!(
            props["quarantine_reason"].as_str(),
            env.metadata.get("quarantine_reason").map(String::as_str),
            "{builder}"
        );
        assert_eq!(props["quarantined"], "true", "{builder}");
    }
}

// --- ADR-200 §7 ordered inbound disposition ---

/// An enabled outbound policy with a fixed test key, so the channel verifies tokens the same
/// way the outbound path mints them.
fn enabled_policy() -> Arc<OutboundEmailPolicy> {
    Arc::new(
        OutboundEmailPolicy::configured_for_tests(
            vec!["allowed@example.com".into()],
            vec!["actor:sender".into()],
            100,
        )
        .expect("test policy"),
    )
}

/// A scripted step-6 suppression check over a sender address.
type ScriptedPredicate = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// The test host: the external-email services the channel takes from its daemon, wired over
/// the same khive-runtime policy and khive-pack-comm store the production host uses.
struct TestExternalHost {
    policy: Option<Arc<OutboundEmailPolicy>>,
    runtime: Option<KhiveRuntime>,
    /// What [`ExternalEmailHost::external_senders_configured`] reports. Derived from the
    /// policy by [`Self::over_runtime`]; fixed directly by [`Self::scripted`].
    configured: bool,
    /// A scripted step-6 check, used instead of the store by [`Self::scripted`].
    predicate: Option<ScriptedPredicate>,
    /// Counts every call to [`ExternalEmailHost::inbound_suppression`].
    inbound_reads: Arc<AtomicUsize>,
}

impl TestExternalHost {
    fn over_runtime(
        policy: Option<Arc<OutboundEmailPolicy>>,
        runtime: Option<KhiveRuntime>,
    ) -> Self {
        let configured = policy
            .as_deref()
            .is_some_and(OutboundEmailPolicy::has_external_senders);
        Self {
            policy,
            runtime,
            configured,
            predicate: None,
            inbound_reads: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// A host with a fixed configured report and a scripted step-6 predicate, for the tests
    /// that drive the check directly rather than through the store.
    fn scripted(
        configured: bool,
        predicate: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            policy: None,
            runtime: None,
            configured,
            predicate: Some(Arc::new(predicate)),
            inbound_reads: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl ExternalEmailHost for TestExternalHost {
    fn external_senders_configured(&self) -> bool {
        self.configured
    }

    fn verify_unsubscribe_token(&self, token: &str) -> Option<String> {
        self.policy.as_deref()?.verify_unsubscribe_token(token)
    }

    async fn lookup_thread_owner(
        &self,
        _candidate: &str,
    ) -> Result<Option<crate::external_inbound::ThreadOwner>, ChannelError> {
        Ok(None)
    }

    async fn inbound_suppression(&self) -> Result<InboundSuppression, ChannelError> {
        self.inbound_reads.fetch_add(1, Ordering::SeqCst);
        if let Some(predicate) = &self.predicate {
            let predicate = Arc::clone(predicate);
            return Ok(Arc::new(move |sender: &str| predicate(sender)));
        }
        let Some(runtime) = self.runtime.as_ref() else {
            return Ok(Arc::new(|_: &str| false));
        };
        let entries = khive_pack_comm::handlers::load_inbound_email_suppressions(runtime)
            .await
            .map_err(|error| {
                ChannelError::Transport(format!("inbound suppression read failed: {error}"))
            })?;
        Ok(Arc::new(move |sender: &str| {
            khive_runtime::inbound_email_is_suppressed(sender, &entries)
        }))
    }

    async fn record_mailto_unsubscribe(&self, recipient_key: &str) -> Result<bool, ChannelError> {
        let Some(runtime) = self.runtime.as_ref() else {
            warn!(
                "mailto unsubscribe: no comm store is wired to this channel; suppression entry not recorded"
            );
            return Ok(false);
        };
        let actor = runtime
            .config()
            .actor_id
            .clone()
            .unwrap_or_else(|| "channel:email".to_string());
        khive_pack_comm::handlers::record_mailto_unsubscribe_suppression(
            runtime,
            recipient_key,
            &actor,
        )
        .await
        .map_err(|error| ChannelError::Transport(format!("{error}")))
    }
}

/// Build a channel with a test host over the given policy and comm runtime, so the mailto
/// step can record its suppression entry and the step-6 check can read the store.
fn build_channel_with_store(
    maintainer: &str,
    emails: Vec<RawEmail>,
    policy: Option<Arc<OutboundEmailPolicy>>,
    comm: Option<KhiveRuntime>,
) -> EmailChannel {
    build_channel_with_host(
        maintainer,
        emails,
        TestExternalHost::over_runtime(policy, comm),
    )
}

/// Build a channel wired to `host`, so tests can inspect the host's read counter.
fn build_channel_with_host(
    maintainer: &str,
    emails: Vec<RawEmail>,
    host: TestExternalHost,
) -> EmailChannel {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let smtp = SmtpSender::with_connector(RecordingSmtp {
        calls: calls.clone(),
    });
    let imap = ImapFetcher::with_connector(FixedImap { emails });
    let channel = EmailChannel::with_connectors(make_config(maintainer), smtp, imap);
    channel.with_external_host(Arc::new(host))
}

/// The step-6 check the store-backed host would produce, for the tests that pass entries
/// straight to `disposition`.
fn inbound_predicate(entries: &[khive_runtime::EmailSuppression]) -> InboundSuppression {
    let entries = entries.to_vec();
    Arc::new(move |sender: &str| khive_runtime::inbound_email_is_suppressed(sender, &entries))
}

/// An in-memory comm runtime whose suppression schema exists (an always-empty read creates it).
async fn suppression_runtime() -> KhiveRuntime {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    khive_pack_comm::handlers::load_inbound_email_suppressions(&runtime)
        .await
        .expect("create suppression schema");
    runtime
}

async fn outbound_recipient_key_rows(runtime: &KhiveRuntime) -> Vec<(String, String)> {
    use khive_storage::SqlStatement;

    let rows = runtime
        .sql()
        .reader()
        .await
        .expect("reader")
        .query_all(SqlStatement {
            sql: "SELECT value, source FROM comm_suppression \
                  WHERE scope = 'outbound' AND kind = 'recipient_key' \
                  ORDER BY value"
                .into(),
            params: vec![],
            label: Some("test-outbound-recipient-keys".into()),
        })
        .await
        .expect("recipient key rows");
    rows.iter()
        .map(|row| {
            let value = match row.get("value") {
                Some(khive_storage::SqlValue::Text(value)) => value.clone(),
                other => panic!("value column: {other:?}"),
            };
            let source = match row.get("source") {
                Some(khive_storage::SqlValue::Text(value)) => value.clone(),
                other => panic!("source column: {other:?}"),
            };
            (value, source)
        })
        .collect()
}

fn auth_failing_email(sender: &str, imap_id: &str) -> RawEmail {
    let mut email = make_email(sender, imap_id);
    // Replace the trusted, self-aligned header with one whose only trusted method passes for a
    // different domain, so domain authentication fails at §7 step 3 (as `unaligned`).
    email.authentication_results = vec![format!(
        "{TEST_AUTHSERV_ID}; dkim=pass header.d=other.example"
    )];
    email
}

// §7 step 2: a verified token is honoured before step 3, so it works even though this
// message's domain authentication fails.
#[tokio::test]
async fn verified_unsubscribe_records_entry_and_drops_message_before_authentication() {
    let policy = enabled_policy();
    let token = policy
        .mint_unsubscribe_token("gone@example.net")
        .expect("mint token");
    let mut email = auth_failing_email("stranger@example.net", "imap:test:0:1");
    email.subject = format!("unsubscribe {token}");
    let runtime = suppression_runtime().await;
    let ch = build_channel_with_store(
        "maintainer@example.com",
        vec![email],
        Some(Arc::clone(&policy)),
        Some(runtime.clone()),
    );

    let envs = ch.poll(Utc::now()).await.unwrap();
    assert!(
        envs.is_empty(),
        "a verified unsubscribe is neither delivered nor quarantined"
    );
    let rows = outbound_recipient_key_rows(&runtime).await;
    assert_eq!(rows.len(), 1, "exactly one recipient_key entry: {rows:?}");
    assert_eq!(rows[0].0, policy.recipient_key("gone@example.net").unwrap());
    assert_eq!(rows[0].1, "mailto_token");
}

// §7 step 2: a token whose characters are altered does not verify and the message continues as
// ordinary mail. Here it reaches step 3 (which fails) and is quarantined `unaligned`, proving
// it was not treated as an opt-out.
#[tokio::test]
async fn tampered_unsubscribe_token_is_ordinary_mail_and_continues_to_authentication() {
    let policy = enabled_policy();
    let token = policy
        .mint_unsubscribe_token("gone@example.net")
        .expect("mint token");
    // Flip a single base64url character, keeping the token's length: the tag no longer checks
    // out, so the candidate must be ignored.
    let mut flipped: Vec<char> = token.chars().collect();
    let middle = flipped.len() / 2;
    flipped[middle] = if flipped[middle] == 'A' { 'B' } else { 'A' };
    let flipped: String = flipped.into_iter().collect();
    assert!(
        policy.verify_unsubscribe_token(&token).is_some(),
        "the minted token verifies"
    );
    assert!(
        policy.verify_unsubscribe_token(&flipped).is_none(),
        "the altered token does not verify"
    );
    let mut email = auth_failing_email("stranger@example.net", "imap:test:0:2");
    email.subject = format!("unsubscribe {flipped}");
    let runtime = suppression_runtime().await;
    let ch = build_channel_with_store(
        "maintainer@example.com",
        vec![email],
        Some(Arc::clone(&policy)),
        Some(runtime.clone()),
    );

    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1, "a tampered token is ordinary mail");
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("unaligned"),
        "the message continued to step 3, where its domain authentication fails"
    );
    assert!(outbound_recipient_key_rows(&runtime).await.is_empty());
}

/// One page holding a single parsed message, with the IMAP progress advanced to its UID.
struct SingleEmailPageImap {
    email: RawEmail,
    seen_progress: Arc<Mutex<Vec<ImapProgress>>>,
}

#[async_trait]
impl ImapConnector for SingleEmailPageImap {
    async fn fetch_page(
        &self,
        _since: DateTime<Utc>,
        _limit: usize,
        progress: ImapProgress,
    ) -> Result<ImapFetchPage, ChannelError> {
        self.seen_progress.lock().unwrap().push(progress);
        Ok(ImapFetchPage {
            emails: vec![SelectedMessage::Email(Box::new(self.email.clone()))],
            next_progress: ImapProgress {
                uid_validity: NonZeroU32::new(3),
                last_seen_uid: NonZeroU32::new(self.email.uid),
            },
        })
    }
}

/// A host whose token verifier works and whose suppression store refuses every write.
struct WriteFailingHost {
    policy: Arc<OutboundEmailPolicy>,
    attempts: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ExternalEmailHost for WriteFailingHost {
    fn external_senders_configured(&self) -> bool {
        false
    }

    fn verify_unsubscribe_token(&self, token: &str) -> Option<String> {
        self.policy.verify_unsubscribe_token(token)
    }

    async fn lookup_thread_owner(
        &self,
        _candidate: &str,
    ) -> Result<Option<crate::external_inbound::ThreadOwner>, ChannelError> {
        Ok(None)
    }

    async fn inbound_suppression(&self) -> Result<InboundSuppression, ChannelError> {
        Ok(Arc::new(|_: &str| false))
    }

    async fn record_mailto_unsubscribe(&self, recipient_key: &str) -> Result<bool, ChannelError> {
        self.attempts
            .lock()
            .unwrap()
            .push(recipient_key.to_string());
        Err(ChannelError::Transport(
            "suppression store unavailable".into(),
        ))
    }
}

// §7 step 1 precedes step 2: a message that could not be parsed is quarantined even when its
// raw bytes carry a subject with a valid token, and no suppression entry is recorded.
#[tokio::test]
async fn malformed_message_with_a_valid_token_is_quarantined_not_unsubscribed() {
    let policy = enabled_policy();
    let token = policy
        .mint_unsubscribe_token("gone@example.net")
        .expect("mint token");
    assert!(
        policy.verify_unsubscribe_token(&token).is_some(),
        "the token in the raw bytes verifies"
    );

    struct MalformedPageImap {
        raw: Vec<u8>,
    }
    #[async_trait]
    impl ImapConnector for MalformedPageImap {
        async fn fetch_page(
            &self,
            _since: DateTime<Utc>,
            _limit: usize,
            _progress: ImapProgress,
        ) -> Result<ImapFetchPage, ChannelError> {
            Ok(ImapFetchPage {
                emails: vec![SelectedMessage::Malformed {
                    uid: 5,
                    imap_external_id: "imap:imap.example.com:3:5".to_string(),
                    reason: MalformedReason::ParseFailure,
                    raw_bytes: Some(self.raw.clone()),
                }],
                next_progress: ImapProgress {
                    uid_validity: NonZeroU32::new(3),
                    last_seen_uid: NonZeroU32::new(5),
                },
            })
        }
    }

    let raw =
        format!("From: stranger@example.net\r\nSubject: unsubscribe {token}\r\n\r\n").into_bytes();
    let runtime = suppression_runtime().await;
    let smtp = SmtpSender::with_connector(RecordingSmtp {
        calls: Arc::new(Mutex::new(Vec::new())),
    });
    let imap = ImapFetcher::with_connector(MalformedPageImap { raw });
    let ch = EmailChannel::with_connectors(make_config("maintainer@example.com"), smtp, imap)
        .with_external_host(Arc::new(TestExternalHost::over_runtime(
            Some(Arc::clone(&policy)),
            Some(runtime.clone()),
        )));

    let page = ch.poll_page(Utc::now(), None).await.unwrap();
    assert_eq!(page.envelopes.len(), 1, "{:?}", page.envelopes);
    assert_eq!(
        page.envelopes[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("parse-failure")
    );
    assert!(outbound_recipient_key_rows(&runtime).await.is_empty());
    assert_eq!(page.next_checkpoint.unwrap().high_water, Some(5));
}

// §7 step 2 through the checkpointed poll: the entry is recorded, nothing is emitted, and the
// checkpoint advances past the message's UID, which is how the poller marks it processed.
#[tokio::test]
async fn verified_unsubscribe_advances_the_checkpoint_past_its_uid() {
    let policy = enabled_policy();
    let token = policy
        .mint_unsubscribe_token("gone@example.net")
        .expect("mint token");
    let mut email = auth_failing_email("stranger@example.net", "imap:test:3:7");
    email.uid = 7;
    email.subject = format!("unsubscribe {token}");
    let runtime = suppression_runtime().await;
    let smtp = SmtpSender::with_connector(RecordingSmtp {
        calls: Arc::new(Mutex::new(Vec::new())),
    });
    let imap = ImapFetcher::with_connector(SingleEmailPageImap {
        email,
        seen_progress: Arc::new(Mutex::new(Vec::new())),
    });
    let ch = EmailChannel::with_connectors(make_config("maintainer@example.com"), smtp, imap)
        .with_external_host(Arc::new(TestExternalHost::over_runtime(
            Some(Arc::clone(&policy)),
            Some(runtime.clone()),
        )));

    let page = ch.poll_page(Utc::now(), None).await.unwrap();
    assert!(page.envelopes.is_empty(), "{:?}", page.envelopes);
    assert_eq!(page.next_checkpoint.unwrap().high_water, Some(7));
    let rows = outbound_recipient_key_rows(&runtime).await;
    assert_eq!(rows.len(), 1, "exactly one recipient_key entry: {rows:?}");
    assert_eq!(rows[0].0, policy.recipient_key("gone@example.net").unwrap());
}

// A failed suppression write returns no checkpoint candidate. Retrying with the same stored
// checkpoint fetches and attempts to record the same verified opt-out again.
#[tokio::test]
async fn failed_unsubscribe_write_fails_the_poll() {
    let policy = enabled_policy();
    let token = policy
        .mint_unsubscribe_token("gone@example.net")
        .expect("mint token");
    let key = policy.recipient_key("gone@example.net").unwrap();
    let raw = format!(
        "From: maintainer@example.com\r\n\
         To: user@example.com\r\n\
         Subject: unsubscribe {token}\r\n\
         Message-ID: <missing-store@example.com>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         unsubscribe\r\n"
    );
    let email = parse_raw_bytes(8, raw.as_bytes(), "imap.example.com", 3)
        .expect("valid synthetic unsubscribe MIME");
    assert_eq!(email.subject, format!("unsubscribe {token}"));
    let smtp_calls = Arc::new(Mutex::new(Vec::new()));
    let smtp = SmtpSender::with_connector(RecordingSmtp {
        calls: Arc::clone(&smtp_calls),
    });
    let seen_progress = Arc::new(Mutex::new(Vec::new()));
    let imap = ImapFetcher::with_connector(SingleEmailPageImap {
        email,
        seen_progress: Arc::clone(&seen_progress),
    });
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let ch = EmailChannel::with_connectors(make_config("maintainer@example.com"), smtp, imap)
        .with_external_host(Arc::new(WriteFailingHost {
            policy,
            attempts: Arc::clone(&attempts),
        }));
    let since = Utc::now();
    let progress = ImapProgress {
        uid_validity: NonZeroU32::new(3),
        last_seen_uid: NonZeroU32::new(7),
    };
    let checkpoint = StoredChannelCheckpoint {
        checkpoint: ch.encode_checkpoint(progress),
        committed_at: since,
    };

    for _ in 0..2 {
        let error = ch.poll_page(since, Some(&checkpoint)).await.unwrap_err();
        assert!(
            matches!(&error, ChannelError::Transport(message)
                if message.contains("mailto unsubscribe suppression write failed")
                    && message.contains("suppression store unavailable")),
            "{error:?}"
        );
    }
    assert_eq!(
        *seen_progress.lock().unwrap(),
        vec![progress, progress],
        "both fetches start at the same committed UID"
    );
    assert_eq!(*attempts.lock().unwrap(), vec![key.clone(), key]);
    assert!(smtp_calls.lock().unwrap().is_empty());
}

// A §7 step-2 message is a machine record only: no actor-visible envelope, no quarantine row,
// and the poll cursor advancing past its UID is how the poller marks it processed.
#[tokio::test]
async fn verified_unsubscribe_is_not_quarantined_and_not_delivered() {
    let policy = enabled_policy();
    let token = policy
        .mint_unsubscribe_token("gone@example.net")
        .expect("mint token");
    let mut email = make_email("maintainer@example.com", "imap:test:0:3");
    email.subject = format!("unsubscribe {token}");
    let ch = build_channel_with_store(
        "maintainer@example.com",
        vec![email],
        Some(Arc::clone(&policy)),
        None,
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert!(envs.is_empty(), "{envs:?}");
}

// §7 step 5 / A1: with external senders unset the channel holds no verifier, so even a subject
// carrying a token mintable elsewhere is ordinary mail and the reader is unchanged -- an
// authenticated non-maintainer is quarantined `off-allowlist`.
#[tokio::test]
async fn senders_unset_keeps_off_allowlist_outcome() {
    let minting = enabled_policy();
    let token = minting
        .mint_unsubscribe_token("gone@example.com")
        .expect("mint token");
    let mut email = make_email("stranger@example.com", "imap:test:0:4");
    email.subject = format!("unsubscribe {token}");
    let ch = build_channel_with_store("maintainer@example.com", vec![email], None, None);
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("off-allowlist")
    );
    assert_eq!(
        envs[0].metadata.get("quarantined").map(String::as_str),
        Some("true")
    );
}

// §7 step 6: with external senders unset the channel never reads the inbound suppression list,
// so a host that would block every sender is never consulted and the message keeps the
// `off-allowlist` outcome.
#[tokio::test]
async fn external_senders_unset_never_reads_the_inbound_list() {
    let host = TestExternalHost::scripted(false, |_| true);
    let reads = Arc::clone(&host.inbound_reads);
    let ch = build_channel_with_host(
        "maintainer@example.com",
        vec![make_email("stranger@example.com", "imap:test:0:5")],
        host,
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("off-allowlist"),
        "with external senders unset the outcome stays off-allowlist, never blocked"
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "the inbound list is not read while external senders are unset"
    );
}

// §7 step 6: with external senders configured, the loaded predicate quarantines the listed
// sender as `blocked` and leaves another non-maintainer at `off-allowlist`.
#[tokio::test]
async fn external_host_suppression_blocks_a_listed_sender() {
    let host = TestExternalHost::scripted(true, |sender| sender == "listed@example.com");
    let reads = Arc::clone(&host.inbound_reads);
    let ch = build_channel_with_host(
        "maintainer@example.com",
        vec![
            make_email("listed@example.com", "imap:test:0:6"),
            make_email("other@example.com", "imap:test:0:7"),
        ],
        host,
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 2);
    assert_eq!(
        envs[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("blocked")
    );
    assert_eq!(
        envs[1]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("off-allowlist"),
        "a sender the predicate does not match stays off-allowlist"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

// §7 step 6: an authenticated stranger matching an inbound address entry is quarantined
// `blocked`.
#[tokio::test]
async fn inbound_address_suppression_quarantines_blocked() {
    let entries = [khive_runtime::EmailSuppression {
        scope: khive_runtime::EmailSuppressionScope::Inbound,
        kind: khive_runtime::EmailSuppressionKind::Address,
        value: "stranger@example.com".into(),
    }];
    let ch = build_channel_with_store(
        "maintainer@example.com",
        vec![],
        Some(enabled_policy()),
        None,
    );
    let (envelopes, _) = ch
        .disposition(
            vec![SelectedMessage::Email(Box::new(make_email(
                "stranger@example.com",
                "imap:test:0:8",
            )))],
            Some(&inbound_predicate(&entries)),
        )
        .await
        .unwrap();
    assert_eq!(envelopes.len(), 1);
    assert_eq!(
        envelopes[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("blocked")
    );
}

// §7 step 6: an inbound domain entry blocks the domain exactly, never a subdomain.
#[tokio::test]
async fn inbound_domain_suppression_blocks_domain_but_not_subdomain() {
    let entries = [khive_runtime::EmailSuppression {
        scope: khive_runtime::EmailSuppressionScope::Inbound,
        kind: khive_runtime::EmailSuppressionKind::Domain,
        value: "example.com".into(),
    }];
    let predicate = inbound_predicate(&entries);
    let ch = build_channel_with_store(
        "maintainer@example.com",
        vec![],
        Some(enabled_policy()),
        None,
    );
    let (blocked, _) = ch
        .disposition(
            vec![SelectedMessage::Email(Box::new(make_email(
                "someone@example.com",
                "imap:test:0:9",
            )))],
            Some(&predicate),
        )
        .await
        .unwrap();
    assert_eq!(
        blocked[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("blocked")
    );

    let (sub, _) = ch
        .disposition(
            vec![SelectedMessage::Email(Box::new(make_email(
                "someone@sub.example.com",
                "imap:test:0:10",
            )))],
            Some(&predicate),
        )
        .await
        .unwrap();
    assert_eq!(
        sub[0].metadata.get("quarantine_reason").map(String::as_str),
        Some("off-allowlist"),
        "a subdomain sender is not blocked by an exact-domain entry"
    );
}

// §7 step 4: the maintainer is attributed and routed as today, unaffected by an inbound entry
// for their address.
#[tokio::test]
async fn maintainer_is_attributed_despite_inbound_suppression() {
    let entries = [khive_runtime::EmailSuppression {
        scope: khive_runtime::EmailSuppressionScope::Inbound,
        kind: khive_runtime::EmailSuppressionKind::Address,
        value: "maintainer@example.com".into(),
    }];
    let ch = build_channel_with_store(
        "maintainer@example.com",
        vec![],
        Some(enabled_policy()),
        None,
    );
    let (envelopes, _) = ch
        .disposition(
            vec![SelectedMessage::Email(Box::new(make_email(
                "maintainer@example.com",
                "imap:test:0:11",
            )))],
            Some(&inbound_predicate(&entries)),
        )
        .await
        .unwrap();
    assert_eq!(envelopes.len(), 1);
    assert_eq!(envelopes[0].from, "email:maintainer@example.com");
    assert!(!envelopes[0].metadata.contains_key("quarantined"));
}

// §7 step 7: with external senders configured and no inbound entry, an authenticated stranger
// keeps today's outcome until screening lands -- `off-allowlist`.
#[tokio::test]
async fn step_seven_case_keeps_off_allowlist() {
    let ch = build_channel_with_store(
        "maintainer@example.com",
        vec![],
        Some(enabled_policy()),
        None,
    );
    let (envelopes, _) = ch
        .disposition(
            vec![SelectedMessage::Email(Box::new(make_email(
                "stranger@example.com",
                "imap:test:0:12",
            )))],
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        envelopes[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("off-allowlist")
    );
}

// §7 step 1: a malformed message is quarantined before any of the later steps run.
#[tokio::test]
async fn malformed_message_is_quarantined_first() {
    let ch = build_channel_with_store(
        "maintainer@example.com",
        vec![],
        Some(enabled_policy()),
        None,
    );
    let (envelopes, _) = ch
        .disposition(
            vec![SelectedMessage::Malformed {
                uid: 1,
                imap_external_id: "imap:test:0:13".to_string(),
                reason: MalformedReason::MissingBody,
                raw_bytes: None,
            }],
            None,
        )
        .await
        .unwrap();
    assert_eq!(envelopes.len(), 1);
    assert_eq!(
        envelopes[0]
            .metadata
            .get("quarantine_reason")
            .map(String::as_str),
        Some("missing-body")
    );
}
include!("external_inbound_tests.rs");
