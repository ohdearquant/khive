use std::collections::BTreeSet;

use serde::{de::DeserializeOwned, Deserialize};

use super::CredentialError;
use crate::engine_config::KhiveConfig;

/// Reads `[[credentials]]` and `[visibility_receipts]` apart from the rest of the config.
/// A refusal names the table or entry and a fixed reason. It never carries a value, a key
/// or a line of the file, because a secret written inline is what these tables refuse.
pub(crate) fn read_tables(raw: &str, config: &mut KhiveConfig) -> Result<(), CredentialError> {
    let mut document: toml::Table = toml::from_str(raw).map_err(|_| refused("config".into()))?;
    if let Some(value) = document.remove("credentials") {
        let toml::Value::Array(entries) = value else {
            return Err(refused("credentials".into()));
        };
        config.credentials = entries
            .into_iter()
            .enumerate()
            .map(|(index, entry)| closed(entry, || format!("credentials[{index}]")))
            .collect::<Result<_, _>>()?;
    }
    if let Some(value) = document.remove("visibility_receipts") {
        config.visibility_receipts = Some(closed(value, || "visibility_receipts".into())?);
    }
    Ok(())
}

fn closed<T: DeserializeOwned>(
    value: toml::Value,
    name: impl FnOnce() -> String,
) -> Result<T, CredentialError> {
    value.try_into().map_err(|_| refused(name()))
}

fn refused(name: String) -> CredentialError {
    CredentialError::InvalidConfig {
        name,
        reason: "unknown, missing or invalid field; values are never shown",
    }
}

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
