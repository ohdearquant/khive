use std::collections::HashMap;
use std::sync::Arc;

/// Boot-resolved recipient policy shared by comm admission and email delivery.
/// An absent policy permits queuing without claiming that delivery is configured.
#[derive(Clone)]
pub struct OutboundEmailPolicy {
    recipients: Option<Arc<[String]>>,
    external_senders: Option<Arc<[String]>>,
    external_daily_cap: Option<u64>,
    /// Recipient keys by normalized address. Key derivation from the recipient-key secret lands
    /// with the unsubscribe token; until then this holds only keys a debug-build test injected.
    recipient_keys: Arc<HashMap<String, String>>,
    external_admission: Arc<tokio::sync::Mutex<()>>,
}

impl Default for OutboundEmailPolicy {
    fn default() -> Self {
        Self {
            recipients: None,
            external_senders: None,
            external_daily_cap: None,
            recipient_keys: Arc::default(),
            external_admission: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
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

    /// The recipient key a `recipient_key` suppression entry matches, when one is known for this
    /// normalized address.
    pub fn recipient_key(&self, normalized_recipient: &str) -> Option<&str> {
        self.recipient_keys
            .get(normalized_recipient)
            .map(String::as_str)
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn configured_for_tests(
        recipients: Vec<String>,
        external_senders: Vec<String>,
        daily_cap: u64,
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
        Ok(policy)
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn with_recipient_key_for_tests(
        mut self,
        recipient: &str,
        recipient_key: &str,
    ) -> Result<Self, String> {
        let normalized = khive_types::email_address::normalize_email_recipient(recipient)
            .ok_or_else(|| "test recipient key requires a valid email address".to_string())?;
        Arc::make_mut(&mut self.recipient_keys).insert(normalized, recipient_key.to_string());
        Ok(self)
    }

    /// Read only public policy inputs, without loading a connector or credentials.
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
        let external_senders = read("KHIVE_EMAIL_EXTERNAL_SENDERS")
            .map_err(|error| format!("{error}; the external email footer is not supported yet"))?;
        let external_daily_cap = read("KHIVE_EMAIL_EXTERNAL_DAILY_CAP").map_err(|error| {
            if external_senders.is_some() {
                format!("{error}; the external email footer is not supported yet")
            } else {
                error
            }
        })?;
        let explicit = read("KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS")?;
        let maintainer = read("KHIVE_EMAIL_MAINTAINER_ADDRESS")?;
        Self::resolve_with_external(
            explicit.as_deref(),
            maintainer.as_deref(),
            external_senders.as_deref(),
            external_daily_cap.as_deref(),
        )
    }

    #[cfg(test)]
    fn resolve(explicit: Option<&str>, maintainer: Option<&str>) -> Result<Self, String> {
        Self::resolve_with_external(explicit, maintainer, None, None)
    }

    fn resolve_with_external(
        explicit: Option<&str>,
        maintainer: Option<&str>,
        external_senders: Option<&str>,
        external_daily_cap: Option<&str>,
    ) -> Result<Self, String> {
        let senders = external_senders
            .map(parse_external_senders)
            .transpose()
            .map_err(|error| format!("{error}; the external email footer is not supported yet"))?;
        let footer_gate = senders.is_some();
        let cap = match external_daily_cap {
            Some(value) => {
                let parsed = value.parse::<u64>().map_err(|_| {
                    let message = "KHIVE_EMAIL_EXTERNAL_DAILY_CAP must be a positive integer";
                    if footer_gate {
                        format!("{message}; the external email footer is not supported yet")
                    } else {
                        message.to_string()
                    }
                })?;
                if parsed == 0 {
                    let message = "KHIVE_EMAIL_EXTERNAL_DAILY_CAP must be a positive integer";
                    return Err(if footer_gate {
                        format!("{message}; the external email footer is not supported yet")
                    } else {
                        message.into()
                    });
                }
                Some(parsed)
            }
            None if senders.is_some() => {
                return Err("KHIVE_EMAIL_EXTERNAL_DAILY_CAP is required when KHIVE_EMAIL_EXTERNAL_SENDERS is set; the external email footer is not supported yet".into());
            }
            None => None,
        };
        if senders.is_some() {
            return Err(
                "KHIVE_EMAIL_EXTERNAL_SENDERS cannot be set because the external email footer is not supported yet".into(),
            );
        }
        let mut policy = Self::resolve_recipients(explicit, maintainer)?;
        policy.external_daily_cap = cap;
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
    fn cloned_policy_keeps_the_same_immutable_recipient_set() {
        let policy = OutboundEmailPolicy::configured(vec!["allowed@example.com".into()]).unwrap();
        let clone = policy.clone();
        assert!(Arc::ptr_eq(
            policy.recipients.as_ref().unwrap(),
            clone.recipients.as_ref().unwrap()
        ));
    }

    #[test]
    fn external_sender_configuration_validates_before_the_footer_gate() {
        let invalid_sender = OutboundEmailPolicy::resolve_with_external(
            None,
            Some("maintainer@example.com"),
            Some("not a valid actor"),
            Some("1"),
        )
        .err()
        .expect("start-up policy is refused");
        assert!(invalid_sender.contains("invalid actor id"));
        assert!(invalid_sender.contains("external email footer is not supported yet"));

        let missing_cap = OutboundEmailPolicy::resolve_with_external(
            None,
            Some("maintainer@example.com"),
            Some("actor:sender"),
            None,
        )
        .err()
        .expect("start-up policy is refused");
        assert!(missing_cap.contains("KHIVE_EMAIL_EXTERNAL_DAILY_CAP is required"));
        assert!(missing_cap.contains("external email footer is not supported yet"));

        let zero_cap = OutboundEmailPolicy::resolve_with_external(
            None,
            Some("maintainer@example.com"),
            Some("actor:sender"),
            Some("0"),
        )
        .err()
        .expect("start-up policy is refused");
        assert!(zero_cap.contains("KHIVE_EMAIL_EXTERNAL_DAILY_CAP must be a positive integer"));
        assert!(zero_cap.contains("external email footer is not supported yet"));

        let valid_sender = OutboundEmailPolicy::resolve_with_external(
            None,
            Some("maintainer@example.com"),
            Some("actor:sender"),
            Some("10"),
        )
        .err()
        .expect("start-up policy is refused");
        assert!(valid_sender.contains("external email footer is not supported yet"));
    }
}
