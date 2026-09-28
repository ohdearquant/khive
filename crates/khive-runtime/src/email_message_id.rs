//! Configuration-bound ownership checks for outbound email Message-IDs.

use uuid::Uuid;

/// Explicit former sending domains that remain valid for redelivery and replies.
pub const HISTORICAL_DOMAINS_ENV: &str = "KHIVE_EMAIL_MESSAGE_ID_HISTORICAL_DOMAINS";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmailMessageIdDomains {
    mailbox: String,
    current: String,
    historical: Vec<String>,
}

impl EmailMessageIdDomains {
    pub fn from_mailbox_and_history(mailbox: &str, historical: &str) -> Result<Self, String> {
        let current = mailbox.split('@').nth(1).unwrap_or("localhost");
        validate_domain(current)?;
        let mut former = Vec::new();
        for raw in historical
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            validate_domain(raw)?;
            if raw.eq_ignore_ascii_case("localhost") && !current.eq_ignore_ascii_case("localhost") {
                return Err("localhost may be used only for the selected no-domain mailbox".into());
            }
            if raw != current && !former.iter().any(|domain| domain == raw) {
                former.push(raw.to_string());
            }
        }
        Ok(Self {
            mailbox: mailbox.to_string(),
            current: current.to_string(),
            historical: former,
        })
    }

    /// The pack and the channel read the same deployment configuration. A pack
    /// without an email mailbox has no authority to accept stored email IDs.
    pub fn from_env() -> Result<Option<Self>, String> {
        let mailbox =
            optional_env("KHIVE_EMAIL_MAILBOX")?.or(optional_env("KHIVE_EMAIL_USERNAME")?);
        let historical = optional_env(HISTORICAL_DOMAINS_ENV)?.unwrap_or_default();
        match mailbox {
            Some(mailbox) => Self::from_mailbox_and_history(&mailbox, &historical).map(Some),
            None if historical.is_empty() => Ok(None),
            None => Err(format!(
                "{HISTORICAL_DOMAINS_ENV} requires KHIVE_EMAIL_MAILBOX or KHIVE_EMAIL_USERNAME"
            )),
        }
    }

    pub fn current(&self) -> &str {
        &self.current
    }

    pub fn mailbox(&self) -> &str {
        &self.mailbox
    }

    pub fn mint(&self, note_id: Uuid) -> String {
        format!("<{note_id}@{}>", self.current)
    }

    /// Equality against the row's own canonical UUID is intentional: a
    /// parseable ID borrowed from another row is not this row's claim.
    pub fn verify(&self, note_id: Uuid, stored: &str) -> bool {
        let own_id = note_id.as_hyphenated();
        std::iter::once(self.current.as_str())
            .chain(self.historical.iter().map(String::as_str))
            .any(|domain| stored == format!("<{own_id}@{domain}>"))
    }

    pub fn verifies_channel_slug(&self, slug: Option<&str>) -> bool {
        slug.is_none_or(|slug| slug == self.mailbox)
    }
}

fn optional_env(name: &str) -> Result<Option<String>, String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(Some(value)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(format!("{name} must contain valid Unicode text"))
        }
    }
}

fn validate_domain(domain: &str) -> Result<(), String> {
    let valid = !domain.is_empty()
        && domain.len() <= 253
        && domain.is_ascii()
        && domain.bytes().all(|byte| {
            byte.is_ascii_alphabetic() || byte.is_ascii_digit() || byte == b'-' || byte == b'.'
        })
        && domain
            .split('.')
            .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid configured email Message-ID domain {domain:?}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::EmailMessageIdDomains;
    use uuid::Uuid;

    #[test]
    fn message_id_ownership_requires_own_uuid_and_configured_domain() {
        let domains = EmailMessageIdDomains::from_mailbox_and_history(
            "sender@current.example",
            "former.example",
        )
        .unwrap();
        let owner = Uuid::new_v4();
        let copier = Uuid::new_v4();
        assert!(domains.verify(owner, &domains.mint(owner)));
        assert!(domains.verify(owner, &format!("<{owner}@former.example>")));
        assert!(!domains.verify(copier, &format!("<{owner}@former.example>")));
        assert!(!domains.verify(owner, &format!("<{owner}@unconfigured.example>")));
        assert!(!domains.verify(owner, &format!("{owner}@current.example")));
        assert!(!domains.verify(
            owner,
            &format!("<{}@current.example>", owner.to_string().to_uppercase())
        ));
        assert!(domains.verifies_channel_slug(Some("sender@current.example")));
        assert!(!domains.verifies_channel_slug(Some("other@current.example")));
    }

    #[test]
    fn localhost_is_only_the_selected_no_domain_mailbox_fallback() {
        let domains =
            EmailMessageIdDomains::from_mailbox_and_history("sender@example.com", "").unwrap();
        let id = Uuid::new_v4();
        assert!(!domains.verify(id, &format!("<{id}@localhost>")));
        assert!(
            EmailMessageIdDomains::from_mailbox_and_history("sender@example.com", "localhost")
                .is_err()
        );
        assert!(
            EmailMessageIdDomains::from_mailbox_and_history("sender@example.com", "LOCALHOST")
                .is_err()
        );
        let fallback = EmailMessageIdDomains::from_mailbox_and_history("sender", "").unwrap();
        assert!(fallback.verify(id, &format!("<{id}@localhost>")));
    }

    #[test]
    fn configured_domain_spelling_is_preserved_for_replay() {
        let domains = EmailMessageIdDomains::from_mailbox_and_history(
            "sender@Current.Example",
            "Former.Example",
        )
        .unwrap();
        let id = Uuid::new_v4();
        assert_eq!(domains.mint(id), format!("<{id}@Current.Example>"));
        assert!(domains.verify(id, &format!("<{id}@Former.Example>")));
        assert!(!domains.verify(id, &format!("<{id}@former.example>")));
    }
}
