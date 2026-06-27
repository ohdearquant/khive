//! Environment-only configuration for the email channel.
//!
//! All settings are read from environment variables. No filesystem config is
//! loaded. Missing required variables produce an error at construction time.

use crate::connector::MailAddress;
use khive_channel::ChannelError;

/// Configuration for the SMTP sender and IMAP fetcher.
///
/// Build via [`EmailChannelConfig::from_env`]. All fields are sourced
/// exclusively from environment variables; no defaults are provided for
/// credentials or host names.
#[derive(Clone, Debug)]
pub struct EmailChannelConfig {
    /// SMTP relay host (e.g. `smtp.example.com`).
    pub smtp_host: String,
    /// SMTP port. Defaults to 587 (STARTTLS) when `KHIVE_EMAIL_SMTP_PORT` is unset.
    pub smtp_port: u16,
    /// IMAP server host (e.g. `imap.example.com`).
    pub imap_host: String,
    /// IMAP port. Defaults to 993 (TLS) when `KHIVE_EMAIL_IMAP_PORT` is unset.
    pub imap_port: u16,
    /// Login username for both SMTP and IMAP.
    pub username: String,
    /// Login password for both SMTP and IMAP. Never logged or stored.
    pub password: String,
    /// The single authorized maintainer address. Inbound messages from any other
    /// sender are rejected before ingestion.
    pub maintainer_address: MailAddress,
}

impl EmailChannelConfig {
    /// Load configuration from environment variables.
    ///
    /// Required variables:
    /// - `KHIVE_EMAIL_SMTP_HOST`
    /// - `KHIVE_EMAIL_IMAP_HOST`
    /// - `KHIVE_EMAIL_USERNAME`
    /// - `KHIVE_EMAIL_PASSWORD`
    /// - `KHIVE_EMAIL_MAINTAINER_ADDRESS`
    ///
    /// Optional variables with defaults:
    /// - `KHIVE_EMAIL_SMTP_PORT` (default `587`)
    /// - `KHIVE_EMAIL_IMAP_PORT` (default `993`)
    pub fn from_env() -> Result<Self, ChannelError> {
        let smtp_host = require_env("KHIVE_EMAIL_SMTP_HOST")?;
        let smtp_port = optional_port("KHIVE_EMAIL_SMTP_PORT", 587)?;
        let imap_host = require_env("KHIVE_EMAIL_IMAP_HOST")?;
        let imap_port = optional_port("KHIVE_EMAIL_IMAP_PORT", 993)?;
        let username = require_env("KHIVE_EMAIL_USERNAME")?;
        let password = require_env("KHIVE_EMAIL_PASSWORD")?;
        let maintainer_raw = require_env("KHIVE_EMAIL_MAINTAINER_ADDRESS")?;
        let maintainer_address = MailAddress::parse(&maintainer_raw).ok_or_else(|| {
            ChannelError::Config(format!(
                "KHIVE_EMAIL_MAINTAINER_ADDRESS is not a valid RFC 5322 address: {maintainer_raw:?}"
            ))
        })?;

        Ok(Self {
            smtp_host,
            smtp_port,
            imap_host,
            imap_port,
            username,
            password,
            maintainer_address,
        })
    }
}

fn require_env(key: &str) -> Result<String, ChannelError> {
    std::env::var(key).map_err(|_| {
        ChannelError::Config(format!("required environment variable {key:?} is not set"))
    })
}

fn optional_port(key: &str, default: u16) -> Result<u16, ChannelError> {
    match std::env::var(key) {
        Err(_) => Ok(default),
        Ok(v) => v.parse::<u16>().map_err(|_| {
            ChannelError::Config(format!(
                "environment variable {key:?} must be a valid port number (1-65535), got: {v:?}"
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_required_env_returns_error() {
        // Ensure the key is absent (it may not be set in CI; use a unique key).
        std::env::remove_var("KHIVE_EMAIL_SMTP_HOST_TEST_MISSING");
        // from_env() will fail because KHIVE_EMAIL_SMTP_HOST is absent in a clean env.
        // We test require_env directly to avoid environment pollution.
        let result = require_env("KHIVE_EMAIL_SMTP_HOST_TEST_MISSING");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("KHIVE_EMAIL_SMTP_HOST_TEST_MISSING"));
    }

    #[test]
    fn optional_port_uses_default_when_absent() {
        std::env::remove_var("KHIVE_EMAIL_SMTP_PORT_TEST");
        let port = optional_port("KHIVE_EMAIL_SMTP_PORT_TEST", 587).unwrap();
        assert_eq!(port, 587);
    }

    #[test]
    fn optional_port_parses_set_value() {
        std::env::set_var("KHIVE_EMAIL_SMTP_PORT_TEST2", "465");
        let port = optional_port("KHIVE_EMAIL_SMTP_PORT_TEST2", 587).unwrap();
        assert_eq!(port, 465);
        std::env::remove_var("KHIVE_EMAIL_SMTP_PORT_TEST2");
    }

    #[test]
    fn optional_port_rejects_invalid_value() {
        std::env::set_var("KHIVE_EMAIL_SMTP_PORT_TEST3", "notaport");
        let result = optional_port("KHIVE_EMAIL_SMTP_PORT_TEST3", 587);
        assert!(result.is_err());
        std::env::remove_var("KHIVE_EMAIL_SMTP_PORT_TEST3");
    }
}
