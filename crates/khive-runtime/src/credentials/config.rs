use std::collections::BTreeSet;

use serde::Deserialize;

use super::CredentialError;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    Header,
    Basic,
    CookieJar,
    SigningKey,
}

/// Closed named references. Material is resolved by providers and never stored in config.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConfig {
    pub name: String,
    pub kind: CredentialKind,
    pub provider: String,
    pub env_var: Option<String>,
    pub header: Option<String>,
}

impl CredentialConfig {
    pub fn validate_all(entries: &[Self]) -> Result<(), CredentialError> {
        let mut names = BTreeSet::new();
        for entry in entries {
            let invalid = |reason| CredentialError::InvalidConfig {
                name: entry.name.clone(),
                reason,
            };
            if entry.name.is_empty() || !names.insert(&entry.name) {
                return Err(invalid("credential names must be nonempty and unique"));
            }
            if entry.provider.is_empty() {
                return Err(invalid("provider must be nonempty"));
            }
            if entry.provider == "env" {
                if entry
                    .env_var
                    .as_deref()
                    .is_none_or(|name| name.is_empty() || name.contains(['=', '\0']))
                {
                    return Err(invalid("env provider requires a valid env_var name"));
                }
            } else if entry.env_var.is_some() {
                return Err(invalid("env_var is only supported by the env provider"));
            }
            match (entry.kind, entry.header.as_deref()) {
                (CredentialKind::Header, Some(header)) if valid_header_name(header) => {}
                (CredentialKind::Header, _) => {
                    return Err(invalid("header credentials require a valid header name"));
                }
                (_, Some(_)) => return Err(invalid("header is only valid for header credentials")),
                (_, None) => {}
            }
        }
        Ok(())
    }
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// Receipt key IDs and credential references; this table contains no key bytes.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisibilityReceiptConfig {
    #[serde(default)]
    pub keys: Vec<VisibilityReceiptKeyConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisibilityReceiptKeyConfig {
    pub id: String,
    pub credential: String,
    #[serde(default)]
    pub encrypt: bool,
}

impl VisibilityReceiptConfig {
    pub fn validate(&self, credentials: &[CredentialConfig]) -> Result<(), CredentialError> {
        CredentialConfig::validate_all(credentials)?;
        let mut ids = BTreeSet::new();
        let mut encrypting = 0;
        for key in &self.keys {
            let invalid = |reason| CredentialError::InvalidConfig {
                name: "visibility_receipts".to_owned(),
                reason,
            };
            if !(1..=64).contains(&key.id.len())
                || !key
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            {
                return Err(invalid(
                    "key IDs require 1..64 ASCII letters, digits, '.', '_' or '-'",
                ));
            }
            if !ids.insert(&key.id) {
                return Err(invalid("key IDs must be unique"));
            }
            let Some(credential) = credentials
                .iter()
                .find(|entry| entry.name == key.credential)
            else {
                return Err(CredentialError::UnknownCredential {
                    name: key.credential.clone(),
                });
            };
            if credential.kind != CredentialKind::SigningKey {
                return Err(invalid(
                    "receipt keys must reference signing_key credentials",
                ));
            }
            encrypting += usize::from(key.encrypt);
        }
        if encrypting != 1 {
            return Err(CredentialError::InvalidConfig {
                name: "visibility_receipts".to_owned(),
                reason: "exactly one key must have encrypt = true",
            });
        }
        Ok(())
    }
}
