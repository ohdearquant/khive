use std::sync::Arc;

/// Boot-resolved recipient policy shared by comm admission and email delivery.
/// An absent policy permits queuing without claiming that delivery is configured.
#[derive(Clone, Default)]
pub struct OutboundEmailPolicy {
    recipients: Option<Arc<[String]>>,
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
        let explicit = read("KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS")?;
        let maintainer = read("KHIVE_EMAIL_MAINTAINER_ADDRESS")?;
        Self::resolve(explicit.as_deref(), maintainer.as_deref())
    }

    fn resolve(explicit: Option<&str>, maintainer: Option<&str>) -> Result<Self, String> {
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
    fn cloned_policy_keeps_the_same_immutable_recipient_set() {
        let policy = OutboundEmailPolicy::configured(vec!["allowed@example.com".into()]).unwrap();
        let clone = policy.clone();
        assert!(Arc::ptr_eq(
            policy.recipients.as_ref().unwrap(),
            clone.recipients.as_ref().unwrap()
        ));
    }
}
