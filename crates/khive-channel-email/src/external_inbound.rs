//! External inbound routing and delivery marking (ADR-200 §8.1, §8.2).
//!
//! Three pure helpers, no I/O and no environment access:
//!
//! - [`InboundRoutes`] parses the `address=actor` routing table from a caller
//!   supplied string.
//! - [`route_inbound`] decides where an inbound message is delivered: to a
//!   verified outbound owner, to a configured recipient alias, or to the
//!   external inbound actor.
//! - [`external_delivery_marking`] builds the delivery metadata that marks a
//!   message as external and untrusted.
//!
//! The channel's external disposition calls these helpers after authentication
//! and suppression. The host verifies ownership before the pure routing decision.

use std::collections::HashSet;

use mail_parser::{HeaderValue, MessageParser};
use serde_json::{json, Value};

use crate::connector::{MailAddress, RawEmail};

/// `origin` metadata value for a delivered external message (ADR-200 §8.1).
pub const ORIGIN_EXTERNAL: &str = "external";

/// `trust` metadata value for a delivered external message (ADR-200 §8.1).
pub const TRUST_UNTRUSTED: &str = "untrusted";

/// A malformed entry in the `address=actor` routing table.
///
/// `position` is 1-based and names the offending comma-separated pair. A
/// malformed pair is never silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InboundRoutesError {
    /// The pair has no `=` separator.
    #[error("inbound route pair {position} is missing an '=' separator")]
    MissingSeparator {
        /// 1-based position of the pair in the comma-separated input.
        position: usize,
    },
    /// The address side is empty.
    #[error("inbound route pair {position} has an empty address")]
    EmptyAddress {
        /// 1-based position of the pair in the comma-separated input.
        position: usize,
    },
    /// The actor side is empty.
    #[error("inbound route pair {position} has an empty actor")]
    EmptyActor {
        /// 1-based position of the pair in the comma-separated input.
        position: usize,
    },
    /// The address side does not parse as an addr-spec.
    #[error("inbound route pair {position} has an unparseable address {address:?}")]
    UnparseableAddress {
        /// 1-based position of the pair in the comma-separated input.
        position: usize,
        /// The raw address text that failed to parse.
        address: String,
    },
    /// The same normalized address appears in more than one pair.
    #[error("inbound route pair {position} repeats address {address}")]
    DuplicateAddress {
        /// 1-based position of the repeated pair.
        position: usize,
        /// The normalized address that repeats.
        address: String,
    },
}

/// Recipient-address routing table (ADR-200 §8.2, `KHIVE_EMAIL_INBOUND_ROUTES`).
///
/// Holds `address=actor` pairs with the address normalized the way
/// [`MailAddress::parse`] normalizes it. Parsing an empty input yields an empty
/// table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InboundRoutes {
    routes: Vec<(String, String)>,
}

impl InboundRoutes {
    /// Parse a comma-separated `address=actor` table.
    ///
    /// Each pair is trimmed. A pair with no `=` separator, an empty address,
    /// an empty actor, an address that does not parse, or an address already
    /// seen returns a typed [`InboundRoutesError`] naming the 1-based position.
    /// Empty (or whitespace-only) input yields an empty table.
    pub fn parse(raw: &str) -> Result<Self, InboundRoutesError> {
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut routes: Vec<(String, String)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for (index, pair) in raw.split(',').enumerate() {
            let position = index + 1;
            let Some((address_raw, actor_raw)) = pair.split_once('=') else {
                return Err(InboundRoutesError::MissingSeparator { position });
            };
            let actor = actor_raw.trim();
            if actor.is_empty() {
                return Err(InboundRoutesError::EmptyActor { position });
            }
            let address_raw = address_raw.trim();
            if address_raw.is_empty() {
                return Err(InboundRoutesError::EmptyAddress { position });
            }
            let Some(address) = MailAddress::parse(address_raw) else {
                return Err(InboundRoutesError::UnparseableAddress {
                    position,
                    address: address_raw.to_string(),
                });
            };
            let normalized = address.as_str().to_string();
            if !seen.insert(normalized.clone()) {
                return Err(InboundRoutesError::DuplicateAddress {
                    position,
                    address: normalized,
                });
            }
            routes.push((normalized, actor.to_string()));
        }
        Ok(Self { routes })
    }

    /// Whether the table contains no routes.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// Number of configured routes.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// The actor configured for a normalized recipient address, if any.
    ///
    /// Matching is exact on the normalized addr-spec produced by
    /// [`MailAddress::parse`]; the Gmail dot/`+tag` canonicalization that
    /// [`MailAddress::matches`] applies is deliberately not used, so two
    /// spellings that reach the same mailbox remain distinct routing keys.
    pub fn actor_for(&self, address: &MailAddress) -> Option<&str> {
        let normalized = address.as_str();
        self.routes
            .iter()
            .find(|(configured, _)| configured == normalized)
            .map(|(_, actor)| actor.as_str())
    }

    /// Iterate the normalized `(address, actor)` pairs in configuration order.
    pub fn pairs(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.routes
            .iter()
            .map(|(address, actor)| (address.as_str(), actor.as_str()))
    }
}

/// The verified owner of an outbound Message-ID: the actor that sent it and the
/// thread a reply belongs to.
///
/// The caller's lookup performs the ownership check (ADR-122 Amendment 3); this
/// module only consumes a positive result. A negative result means no owner is
/// claimed for that candidate, not that the candidate is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadOwner {
    /// Actor that sent the owning outbound message.
    pub actor: String,
    /// Thread the reply should be delivered into.
    pub thread_id: String,
}

/// Where an inbound message is delivered (ADR-200 §8.2).
///
/// Routing headers (`X-Khive-Thread-ID`, `In-Reply-To`, `References`,
/// `Delivered-To`, `To`, `Cc`) are sender-asserted. A forged one changes only
/// *which* actor receives the message; it never changes the message's trust.
/// No variant and no field here derives a trust value from a header: the owner
/// variant is reachable only through the caller's own ownership verification,
/// and a delivered message's trust is fixed at [`TRUST_UNTRUSTED`] by §8.1
/// regardless of which variant is chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundRoute {
    /// A reply to a verified outbound message: deliver into `thread_id` as
    /// `actor`.
    Owner {
        /// Actor that owns the outbound message.
        actor: String,
        /// Thread the reply is delivered into.
        thread_id: String,
    },
    /// A recipient alias matched a configured route.
    RoutedAlias {
        /// Actor configured for the matched address.
        actor: String,
        /// The matched normalized address.
        address: String,
    },
    /// No verified owner and no matching route: the external inbound actor.
    ExternalInbound {
        /// The configured external inbound actor.
        actor: String,
    },
}

/// Decide where an inbound external message is delivered (ADR-200 §8.2).
///
/// Order, exactly as §8.2:
///
/// 1. The first of `X-Khive-Thread-ID`, `In-Reply-To`, then each id in
///    `References` that `owner_lookup` verifies wins ([`InboundRoute::Owner`]).
/// 2. Otherwise the first recipient address across `Delivered-To:`, `To:`, then
///    `Cc:` (that header order; within a header, document order) that matches a
///    configured route ([`InboundRoute::RoutedAlias`]).
/// 3. Otherwise the external inbound actor ([`InboundRoute::ExternalInbound`]).
pub fn route_inbound(
    email: &RawEmail,
    routes: &InboundRoutes,
    external_inbound_actor: &str,
    owner_lookup: impl Fn(&str) -> Option<ThreadOwner>,
) -> InboundRoute {
    for candidate in owner_candidates(email) {
        if let Some(owner) = owner_lookup(&candidate) {
            return InboundRoute::Owner {
                actor: owner.actor,
                thread_id: owner.thread_id,
            };
        }
    }
    for address in recipient_addresses(email) {
        if let Some(actor) = routes.actor_for(&address) {
            return InboundRoute::RoutedAlias {
                actor: actor.to_string(),
                address: address.as_str().to_string(),
            };
        }
    }
    InboundRoute::ExternalInbound {
        actor: external_inbound_actor.to_string(),
    }
}

/// Candidate ids for the owner lookup, in §8.2 order.
///
/// `References` lists ancestors oldest-first (RFC 5322 §3.6.4). Its ids are
/// tried in that document order so the decision is reproducible from the header
/// bytes alone, without assuming a client rewrote the chain: every ancestor in
/// a reply chain belongs to one conversation, and the earliest khive-owned id
/// still identifies that conversation's thread.
pub(crate) fn owner_candidates(email: &RawEmail) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    for value in [email.khive_thread_id(), email.in_reply_to()]
        .into_iter()
        .flatten()
    {
        let value = value.trim();
        if !value.is_empty() {
            candidates.push(value.to_string());
        }
    }
    if let Some(references) = email.references() {
        for token in references.split_whitespace() {
            candidates.push(token.to_string());
        }
    }
    candidates
}

/// Recipient addresses in §8.2 order: `Delivered-To`, then `To`, then `Cc`.
///
/// `raw_bytes` is parsed once and every occurrence of each of the three headers
/// is read in turn, in document order. Re-reading the source is what makes every
/// `Delivered-To` occurrence visible: `RawEmail.headers` keeps only the first
/// occurrence of a name. A message whose bytes do not parse yields no
/// recipients, which falls through to the external inbound actor.
fn recipient_addresses(email: &RawEmail) -> Vec<MailAddress> {
    let parser = MessageParser::default();
    let Some(message) = parser.parse(email.raw_bytes.as_slice()) else {
        return Vec::new();
    };
    let mut addresses = Vec::new();
    for header_name in ["Delivered-To", "To", "Cc"] {
        for header in message.headers() {
            if !header.name().eq_ignore_ascii_case(header_name) {
                continue;
            }
            for candidate in address_strings(header.value()) {
                if let Some(parsed) = MailAddress::parse(&candidate) {
                    addresses.push(parsed);
                }
            }
        }
    }
    addresses
}

/// The address spellings carried by one parsed header value.
///
/// `To`/`Cc` parse to [`HeaderValue::Address`]; `Delivered-To` is an unregistered
/// name the parser keeps as raw text (or a text list), so both shapes are read
/// and each spelling is normalized by [`MailAddress::parse`] at the call site.
fn address_strings(value: &HeaderValue<'_>) -> Vec<String> {
    match value {
        HeaderValue::Address(address) => address
            .iter()
            .filter_map(|addr| addr.address.as_deref())
            .map(str::to_string)
            .collect(),
        HeaderValue::Text(text) => vec![text.to_string()],
        HeaderValue::TextList(list) => list.iter().map(|text| text.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Delivery metadata for a delivered external message (ADR-200 §8.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalDeliveryMarking {
    /// Sender in `email:<normalized address>` form.
    pub sender: String,
    /// `origin` field value; always [`ORIGIN_EXTERNAL`].
    pub origin: &'static str,
    /// `trust` field value; always [`TRUST_UNTRUSTED`].
    pub trust: &'static str,
    /// Screening rule identifiers, never the matched text.
    pub labels: Vec<String>,
}

impl ExternalDeliveryMarking {
    /// Render as a JSON object using the comm ingest metadata key style
    /// (lowercase snake_case keys, JSON values).
    pub fn to_json(&self) -> Value {
        json!({
            "sender": self.sender,
            "origin": self.origin,
            "trust": self.trust,
            "labels": self.labels,
        })
    }
}

/// Why an external message could not be marked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExternalMarkingError {
    /// The normalized sender equals the maintainer address.
    #[error("cannot mark the maintainer address as external: {address}")]
    MaintainerAddress {
        /// The normalized address that matched the maintainer.
        address: String,
    },
    /// The `email:<address>` sender equals the maintainer actor.
    #[error("cannot mark the maintainer actor as external: {sender}")]
    MaintainerActor {
        /// The sender form that matched the maintainer actor.
        sender: String,
    },
    /// The sender address does not parse as an addr-spec.
    #[error("cannot mark an unparseable sender address as external: {address:?}")]
    UnparseableAddress {
        /// The raw address text that failed to parse.
        address: String,
    },
}

/// Build the delivery metadata for an external message (ADR-200 §8.1).
///
/// `from_address` is normalized the way [`MailAddress::parse`] normalizes it and
/// becomes `email:<normalized address>`. `labels` are screening rule
/// identifiers only; the matched text is never accepted here, so it cannot
/// reach the metadata.
///
/// Refuses with a typed error when the resulting sender equals `maintainer_actor`
/// or the normalized address matches `maintainer_address`, so a caller can never
/// mark the maintainer as external.
pub fn external_delivery_marking(
    from_address: &str,
    labels: &[&str],
    maintainer_actor: &str,
    maintainer_address: &str,
) -> Result<ExternalDeliveryMarking, ExternalMarkingError> {
    let address = MailAddress::parse(from_address).ok_or_else(|| {
        ExternalMarkingError::UnparseableAddress {
            address: from_address.to_string(),
        }
    })?;
    let sender = format!("email:{}", address.as_str());
    if !maintainer_actor.is_empty() && sender == maintainer_actor {
        return Err(ExternalMarkingError::MaintainerActor { sender });
    }
    if let Some(maintainer) = MailAddress::parse(maintainer_address) {
        if maintainer.matches(&address) {
            return Err(ExternalMarkingError::MaintainerAddress {
                address: address.as_str().to_string(),
            });
        }
    }
    Ok(ExternalDeliveryMarking {
        sender,
        origin: ORIGIN_EXTERNAL,
        trust: TRUST_UNTRUSTED,
        labels: labels.iter().map(|label| (*label).to_string()).collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;

    /// Build a `RawEmail` the way the IMAP connector does: `headers` keeps the
    /// first occurrence of each header name, `raw_bytes` keeps the full source.
    fn email_with(raw: &str) -> RawEmail {
        let parser = MessageParser::default();
        let parsed = parser.parse(raw.as_bytes()).expect("fixture must parse");
        let mut headers: HashMap<String, String> = HashMap::new();
        for header in parsed.headers() {
            let key = header.name().to_lowercase();
            if let std::collections::hash_map::Entry::Vacant(entry) = headers.entry(key) {
                match header.value() {
                    HeaderValue::Text(value) => {
                        entry.insert(value.to_string());
                    }
                    HeaderValue::TextList(list) => {
                        entry.insert(
                            list.iter()
                                .map(|value| value.as_ref())
                                .collect::<Vec<_>>()
                                .join(" "),
                        );
                    }
                    _ => {}
                }
            }
        }
        RawEmail {
            uid: 1,
            raw_bytes: raw.as_bytes().to_vec(),
            imap_external_id: "imap:test:1:1".to_string(),
            from_addrs: Vec::new(),
            sender_addr: None,
            to: Vec::new(),
            subject: String::new(),
            date: None,
            body_text: None,
            body_html: None,
            headers,
            authentication_results: Vec::new(),
        }
    }

    fn owner(actor: &str, thread: &str) -> ThreadOwner {
        ThreadOwner {
            actor: actor.to_string(),
            thread_id: thread.to_string(),
        }
    }

    fn never(_id: &str) -> Option<ThreadOwner> {
        None
    }

    const ROUTES: &str = "support@example.com=channel:support,team@example.com=channel:team";

    // ── Route parsing ──────────────────────────────────────────────────────

    #[test]
    fn parse_accepts_valid_pairs() {
        let routes = InboundRoutes::parse(ROUTES).unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(
            routes.actor_for(&MailAddress::parse("support@example.com").unwrap()),
            Some("channel:support")
        );
        assert_eq!(
            routes.actor_for(&MailAddress::parse("TEAM@Example.com").unwrap()),
            Some("channel:team")
        );
        assert_eq!(
            routes.actor_for(&MailAddress::parse("other@example.com").unwrap()),
            None
        );
        assert_eq!(
            routes.pairs().collect::<Vec<_>>(),
            vec![
                ("support@example.com", "channel:support"),
                ("team@example.com", "channel:team")
            ]
        );
    }

    #[test]
    fn parse_trims_whitespace_around_pairs_and_sides() {
        let routes = InboundRoutes::parse(
            "  support@example.com = channel:support ,team@example.com= channel:team  ",
        )
        .unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(
            routes.actor_for(&MailAddress::parse("support@example.com").unwrap()),
            Some("channel:support")
        );
        assert_eq!(
            routes.actor_for(&MailAddress::parse("team@example.com").unwrap()),
            Some("channel:team")
        );
    }

    #[test]
    fn parse_normalizes_addresses_and_strips_display_names() {
        let routes =
            InboundRoutes::parse("Support Desk <SUPPORT@Example.com>=channel:support").unwrap();
        assert_eq!(
            routes.actor_for(&MailAddress::parse("support@example.com").unwrap()),
            Some("channel:support")
        );
        assert_eq!(
            routes.pairs().collect::<Vec<_>>(),
            vec![("support@example.com", "channel:support")]
        );
    }

    #[test]
    fn empty_input_yields_no_routes() {
        assert!(InboundRoutes::parse("").unwrap().is_empty());
        assert!(InboundRoutes::parse("   ").unwrap().is_empty());
        assert!(InboundRoutes::parse("\t\n").unwrap().is_empty());
    }

    #[test]
    fn parse_reports_missing_separator_at_pair_position() {
        assert_eq!(
            InboundRoutes::parse("support@example.com").unwrap_err(),
            InboundRoutesError::MissingSeparator { position: 1 }
        );
        assert_eq!(
            InboundRoutes::parse("support@example.com=channel:support,team@example.com")
                .unwrap_err(),
            InboundRoutesError::MissingSeparator { position: 2 }
        );
    }

    #[test]
    fn parse_reports_an_empty_pair_as_a_malformed_pair() {
        // A doubled comma is not silently dropped: the empty pair has no `=` and
        // is reported at its 1-based position.
        assert_eq!(
            InboundRoutes::parse(
                "support@example.com=channel:support,,team@example.com=channel:team"
            )
            .unwrap_err(),
            InboundRoutesError::MissingSeparator { position: 2 }
        );
    }

    #[test]
    fn parse_reports_empty_sides() {
        assert_eq!(
            InboundRoutes::parse("=channel:support").unwrap_err(),
            InboundRoutesError::EmptyAddress { position: 1 }
        );
        assert_eq!(
            InboundRoutes::parse("support@example.com=").unwrap_err(),
            InboundRoutesError::EmptyActor { position: 1 }
        );
        assert_eq!(
            InboundRoutes::parse("support@example.com=   ").unwrap_err(),
            InboundRoutesError::EmptyActor { position: 1 }
        );
    }

    #[test]
    fn parse_reports_unparseable_addresses() {
        assert_eq!(
            InboundRoutes::parse("not-an-address=channel:support").unwrap_err(),
            InboundRoutesError::UnparseableAddress {
                position: 1,
                address: "not-an-address".to_string()
            }
        );
    }

    #[test]
    fn parse_reports_duplicate_address_at_the_repeat_position() {
        assert_eq!(
            InboundRoutes::parse("support@example.com=channel:one,support@example.com=channel:two")
                .unwrap_err(),
            InboundRoutesError::DuplicateAddress {
                position: 2,
                address: "support@example.com".to_string()
            }
        );
        // Normalization makes a case-only difference a duplicate.
        assert_eq!(
            InboundRoutes::parse("Support@Example.com=channel:one,SUPPORT@example.com=channel:two")
                .unwrap_err(),
            InboundRoutesError::DuplicateAddress {
                position: 2,
                address: "support@example.com".to_string()
            }
        );
    }

    // ── Routing (A13) ──────────────────────────────────────────────────────

    #[test]
    fn verified_in_reply_to_reaches_the_owners_thread() {
        let email = email_with(
            "From: alice@example.com\r\n\
             To: support@example.com\r\n\
             In-Reply-To: <out-1@khive.ai>\r\n\
             \r\nbody",
        );
        let routes = InboundRoutes::parse(ROUTES).unwrap();
        let decision = route_inbound(&email, &routes, "channel:external", |id| {
            (id == "out-1@khive.ai").then(|| owner("lambda:khive", "thread-abc"))
        });
        assert_eq!(
            decision,
            InboundRoute::Owner {
                actor: "lambda:khive".to_string(),
                thread_id: "thread-abc".to_string()
            }
        );
    }

    #[test]
    fn unverified_in_reply_to_falls_through_to_the_route() {
        let email = email_with(
            "From: alice@example.com\r\n\
             To: support@example.com\r\n\
             In-Reply-To: <forged@example.com>\r\n\
             \r\nbody",
        );
        let routes = InboundRoutes::parse(ROUTES).unwrap();
        let decision = route_inbound(&email, &routes, "channel:external", never);
        assert_eq!(
            decision,
            InboundRoute::RoutedAlias {
                actor: "channel:support".to_string(),
                address: "support@example.com".to_string()
            }
        );
    }

    #[test]
    fn unverified_in_reply_to_without_a_route_reaches_the_external_actor() {
        let email = email_with(
            "From: alice@example.com\r\n\
             To: other@example.com\r\n\
             In-Reply-To: <forged@example.com>\r\n\
             \r\nbody",
        );
        let routes = InboundRoutes::parse(ROUTES).unwrap();
        let decision = route_inbound(&email, &routes, "channel:external", never);
        assert_eq!(
            decision,
            InboundRoute::ExternalInbound {
                actor: "channel:external".to_string()
            }
        );
    }

    #[test]
    fn verified_thread_id_beats_a_verified_in_reply_to_naming_another_owner() {
        let email = email_with(
            "From: alice@example.com\r\n\
             To: support@example.com\r\n\
             X-Khive-Thread-ID: thread-xyz\r\n\
             In-Reply-To: <out-2@khive.ai>\r\n\
             \r\nbody",
        );
        let routes = InboundRoutes::parse(ROUTES).unwrap();
        let decision = route_inbound(&email, &routes, "channel:external", |id| match id {
            "thread-xyz" => Some(owner("actor:thread-id", "thread-from-header")),
            "out-2@khive.ai" => Some(owner("actor:in-reply-to", "thread-from-parent")),
            _ => None,
        });
        assert_eq!(
            decision,
            InboundRoute::Owner {
                actor: "actor:thread-id".to_string(),
                thread_id: "thread-from-header".to_string()
            }
        );
    }

    #[test]
    fn an_id_found_only_in_references_routes_to_its_owner() {
        let email = email_with(
            "From: alice@example.com\r\n\
             To: support@example.com\r\n\
             References: <root@example.com> <mid@khive.ai>\r\n\
             \r\nbody",
        );
        let routes = InboundRoutes::parse(ROUTES).unwrap();
        let decision = route_inbound(&email, &routes, "channel:external", |id| {
            (id == "mid@khive.ai").then(|| owner("lambda:khive", "thread-refs"))
        });
        assert_eq!(
            decision,
            InboundRoute::Owner {
                actor: "lambda:khive".to_string(),
                thread_id: "thread-refs".to_string()
            }
        );
    }

    #[test]
    fn references_ids_are_tried_in_document_order() {
        let email = email_with(
            "From: alice@example.com\r\n\
             To: support@example.com\r\n\
             References: <root@example.com> <mid@khive.ai>\r\n\
             \r\nbody",
        );
        let routes = InboundRoutes::parse(ROUTES).unwrap();
        let tried: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let decision = route_inbound(&email, &routes, "channel:external", |id| {
            tried.borrow_mut().push(id.to_string());
            None
        });
        assert_eq!(
            decision,
            InboundRoute::RoutedAlias {
                actor: "channel:support".to_string(),
                address: "support@example.com".to_string()
            }
        );
        assert_eq!(
            tried.into_inner(),
            vec!["root@example.com".to_string(), "mid@khive.ai".to_string()]
        );
    }

    #[test]
    fn a_fresh_message_to_a_routed_alias_reaches_that_actors_route() {
        let email = email_with(
            "From: alice@example.com\r\n\
             To: team@example.com\r\n\
             \r\nbody",
        );
        let routes = InboundRoutes::parse(ROUTES).unwrap();
        let decision = route_inbound(&email, &routes, "channel:external", never);
        assert_eq!(
            decision,
            InboundRoute::RoutedAlias {
                actor: "channel:team".to_string(),
                address: "team@example.com".to_string()
            }
        );
    }

    #[test]
    fn an_alias_in_a_later_delivered_to_occurrence_routes() {
        let email = email_with(
            "From: alice@example.com\r\n\
             Delivered-To: personal@example.com\r\n\
             Delivered-To: support@example.com\r\n\
             To: other@example.com\r\n\
             \r\nbody",
        );
        let routes = InboundRoutes::parse(ROUTES).unwrap();
        let decision = route_inbound(&email, &routes, "channel:external", never);
        assert_eq!(
            decision,
            InboundRoute::RoutedAlias {
                actor: "channel:support".to_string(),
                address: "support@example.com".to_string()
            }
        );
    }

    #[test]
    fn cc_matches_only_when_no_earlier_header_matched() {
        let email = email_with(
            "From: alice@example.com\r\n\
             Delivered-To: personal@example.com\r\n\
             To: team@example.com\r\n\
             Cc: support@example.com\r\n\
             \r\nbody",
        );
        let routes = InboundRoutes::parse(ROUTES).unwrap();
        let decision = route_inbound(&email, &routes, "channel:external", never);
        assert_eq!(
            decision,
            InboundRoute::RoutedAlias {
                actor: "channel:team".to_string(),
                address: "team@example.com".to_string()
            }
        );
    }

    #[test]
    fn a_cc_only_alias_routes_when_nothing_earlier_matches() {
        let email = email_with(
            "From: alice@example.com\r\n\
             To: other@example.com\r\n\
             Cc: support@example.com\r\n\
             \r\nbody",
        );
        let routes = InboundRoutes::parse(ROUTES).unwrap();
        let decision = route_inbound(&email, &routes, "channel:external", never);
        assert_eq!(
            decision,
            InboundRoute::RoutedAlias {
                actor: "channel:support".to_string(),
                address: "support@example.com".to_string()
            }
        );
    }

    #[test]
    fn anything_else_reaches_the_external_inbound_actor() {
        let email = email_with(
            "From: alice@example.com\r\n\
             To: unknown@example.com\r\n\
             \r\nbody",
        );
        let routes = InboundRoutes::parse(ROUTES).unwrap();
        let decision = route_inbound(&email, &routes, "channel:external", never);
        assert_eq!(
            decision,
            InboundRoute::ExternalInbound {
                actor: "channel:external".to_string()
            }
        );
    }

    #[test]
    fn empty_routes_send_everything_to_the_external_actor() {
        let email = email_with("From: alice@example.com\r\nTo: support@example.com\r\n\r\nbody");
        let routes = InboundRoutes::parse("").unwrap();
        let decision = route_inbound(&email, &routes, "channel:external", never);
        assert_eq!(
            decision,
            InboundRoute::ExternalInbound {
                actor: "channel:external".to_string()
            }
        );
    }

    // ── Marking (A14) ──────────────────────────────────────────────────────

    #[test]
    fn marking_carries_sender_origin_trust_and_labels() {
        let marking = external_delivery_marking(
            "Alice <ALICE@Example.com>",
            &["hidden_text", "action_request"],
            "lambda:maintainer",
            "maintainer@example.com",
        )
        .unwrap();
        assert_eq!(marking.sender, "email:alice@example.com");
        assert_eq!(marking.origin, "external");
        assert_eq!(marking.trust, "untrusted");
        assert_eq!(marking.labels, vec!["hidden_text", "action_request"]);

        let json = marking.to_json();
        assert_eq!(json["sender"], "email:alice@example.com");
        assert_eq!(json["origin"], "external");
        assert_eq!(json["trust"], "untrusted");
        assert_eq!(
            json["labels"],
            serde_json::json!(["hidden_text", "action_request"])
        );
    }

    #[test]
    fn marking_labels_are_rule_identifiers_only() {
        // The function accepts only identifiers; there is no argument that
        // could carry the matched text, so the render cannot contain it.
        let marking =
            external_delivery_marking("alice@example.com", &["opt_out"], "actor", "m@example.com")
                .unwrap();
        assert_eq!(marking.labels, vec!["opt_out"]);
        assert_eq!(marking.to_json()["labels"], serde_json::json!(["opt_out"]));
    }

    #[test]
    fn marking_refuses_the_maintainer_address() {
        assert_eq!(
            external_delivery_marking(
                "Maintainer <MAINTAINER@Example.com>",
                &[],
                "lambda:maintainer",
                "maintainer@example.com",
            )
            .unwrap_err(),
            ExternalMarkingError::MaintainerAddress {
                address: "maintainer@example.com".to_string()
            }
        );
    }

    #[test]
    fn marking_refuses_the_maintainer_actor() {
        assert_eq!(
            external_delivery_marking(
                "alice@example.com",
                &[],
                "email:alice@example.com",
                "maintainer@example.com",
            )
            .unwrap_err(),
            ExternalMarkingError::MaintainerActor {
                sender: "email:alice@example.com".to_string()
            }
        );
    }

    #[test]
    fn marking_refuses_a_maintainer_address_spelled_through_gmail_canonicalization() {
        assert_eq!(
            external_delivery_marking(
                "maintainer+inbox@gmail.com",
                &[],
                "lambda:maintainer",
                "main.tainer@gmail.com",
            )
            .unwrap_err(),
            ExternalMarkingError::MaintainerAddress {
                address: "maintainer+inbox@gmail.com".to_string()
            }
        );
    }

    #[test]
    fn marking_refuses_an_unparseable_sender() {
        assert_eq!(
            external_delivery_marking("not-an-address", &[], "actor", "m@example.com").unwrap_err(),
            ExternalMarkingError::UnparseableAddress {
                address: "not-an-address".to_string()
            }
        );
    }
}
