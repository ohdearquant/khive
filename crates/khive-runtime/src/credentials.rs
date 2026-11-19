//! Named credential custody (ADR-192). Resolution never exposes material to packs.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use thiserror::Error;
use zeroize::Zeroizing;

mod config;
pub(crate) use config::read_tables;
pub use config::{
    CredentialConfig, CredentialKind, VisibilityReceiptConfig, VisibilityReceiptKeyConfig,
};

pub(crate) mod receipt_sealer;

#[cfg(test)]
mod config_tests;
#[cfg(test)]
mod tests;

/// Owned secret bytes, zeroized on drop. Only runtime custody consumers can inspect them.
///
/// ```compile_fail
/// let material = khive_runtime::credentials::CredentialMaterial::new(vec![1, 2, 3]);
/// let _bytes: &[u8] = &material.bytes;
/// ```
///
/// ```compile_fail
/// let material = khive_runtime::credentials::CredentialMaterial::new(vec![1, 2, 3]);
/// let _serialized = serde_json::to_string(&material);
/// ```
pub struct CredentialMaterial {
    bytes: Zeroizing<Vec<u8>>,
}

impl CredentialMaterial {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Zeroizing::new(bytes),
        }
    }
}

impl fmt::Debug for CredentialMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialMaterial([REDACTED])")
    }
}

/// Errors contain credential references and fixed reasons, never provider material or causes.
#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("credential {name:?}: unavailable")]
    Unavailable { name: String },
    #[error("credential {name:?}: not configured")]
    UnknownCredential { name: String },
    #[error("credential {name:?}: provider {provider:?} is not registered")]
    UnknownProvider { name: String, provider: String },
    #[error("credential configuration {name:?}: {reason}")]
    InvalidConfig { name: String, reason: &'static str },
    #[error("credential {name:?}: only cookie_jar credentials may be updated")]
    UpdateNotAllowed { name: String },
    #[error("credential {name:?}: provider does not support updates")]
    UpdateUnsupported { name: String },
}

/// Maximum lifetime for a provider's cached resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialCacheLifetime {
    NoCache,
    Process,
}

/// Downstream hosts register custody adapters here; env is the only shipped adapter.
pub trait CredentialProvider: Send + Sync {
    fn resolve(&self, name: &str) -> Result<CredentialMaterial, CredentialError>;

    fn cache_lifetime(&self) -> CredentialCacheLifetime;

    /// The caller supplies replacement material; this does not unwrap resolved material.
    fn update(&self, name: &str, _material: Zeroizing<Vec<u8>>) -> Result<(), CredentialError> {
        Err(CredentialError::UpdateUnsupported {
            name: name.to_owned(),
        })
    }
}

struct EnvCredentialProvider {
    variables: BTreeMap<String, String>,
}

impl CredentialProvider for EnvCredentialProvider {
    fn resolve(&self, name: &str) -> Result<CredentialMaterial, CredentialError> {
        let unavailable = || CredentialError::Unavailable {
            name: name.to_owned(),
        };
        let variable = self.variables.get(name).ok_or_else(unavailable)?;
        let value = std::env::var_os(variable).ok_or_else(unavailable)?;
        let bytes = Zeroizing::new(value.into_encoded_bytes());
        if bytes.is_empty() || std::str::from_utf8(&bytes).is_err() {
            return Err(unavailable());
        }
        Ok(CredentialMaterial { bytes })
    }

    fn cache_lifetime(&self) -> CredentialCacheLifetime {
        CredentialCacheLifetime::NoCache
    }
}

/// Validated references plus provider implementations, without serialized secret values.
pub struct CredentialRegistry {
    declarations: BTreeMap<String, CredentialConfig>,
    providers: BTreeMap<String, Arc<dyn CredentialProvider>>,
}

impl CredentialRegistry {
    pub fn new(declarations: Vec<CredentialConfig>) -> Result<Self, CredentialError> {
        CredentialConfig::validate_all(&declarations)?;
        let variables = declarations
            .iter()
            .filter(|entry| entry.provider == "env")
            .map(|entry| {
                (
                    entry.name.clone(),
                    entry.env_var.clone().expect("validated env reference"),
                )
            })
            .collect();
        let mut providers: BTreeMap<String, Arc<dyn CredentialProvider>> = BTreeMap::new();
        providers.insert(
            "env".to_owned(),
            Arc::new(EnvCredentialProvider { variables }),
        );
        Ok(Self {
            declarations: declarations
                .into_iter()
                .map(|entry| (entry.name.clone(), entry))
                .collect(),
            providers,
        })
    }

    /// Registration cannot replace an existing provider, including the built-in env adapter.
    pub fn register_provider(
        &mut self,
        name: String,
        provider: Arc<dyn CredentialProvider>,
    ) -> Result<(), CredentialError> {
        if name.is_empty() || self.providers.contains_key(&name) {
            return Err(CredentialError::InvalidConfig {
                name,
                reason: "provider name must be nonempty and not already registered",
            });
        }
        self.providers.insert(name, provider);
        Ok(())
    }

    pub(crate) fn declarations(&self) -> Vec<CredentialConfig> {
        self.declarations.values().cloned().collect()
    }

    pub fn kind(&self, name: &str) -> Result<CredentialKind, CredentialError> {
        Ok(self.declaration(name)?.kind)
    }

    pub fn cache_lifetime(&self, name: &str) -> Result<CredentialCacheLifetime, CredentialError> {
        Ok(self.provider(self.declaration(name)?)?.cache_lifetime())
    }

    pub fn resolve(&self, name: &str) -> Result<CredentialMaterial, CredentialError> {
        self.provider(self.declaration(name)?)?
            .resolve(name)
            .map_err(|_| CredentialError::Unavailable {
                name: name.to_owned(),
            })
    }

    pub fn update(&self, name: &str, material: Vec<u8>) -> Result<(), CredentialError> {
        let material = Zeroizing::new(material);
        let declaration = self.declaration(name)?;
        if declaration.kind != CredentialKind::CookieJar {
            return Err(CredentialError::UpdateNotAllowed {
                name: name.to_owned(),
            });
        }
        self.provider(declaration)?
            .update(name, material)
            .map_err(|error| match error {
                CredentialError::UpdateUnsupported { .. } => CredentialError::UpdateUnsupported {
                    name: name.to_owned(),
                },
                _ => CredentialError::Unavailable {
                    name: name.to_owned(),
                },
            })
    }

    fn declaration(&self, name: &str) -> Result<&CredentialConfig, CredentialError> {
        self.declarations
            .get(name)
            .ok_or_else(|| CredentialError::UnknownCredential {
                name: name.to_owned(),
            })
    }

    fn provider(
        &self,
        declaration: &CredentialConfig,
    ) -> Result<&Arc<dyn CredentialProvider>, CredentialError> {
        self.providers
            .get(&declaration.provider)
            .ok_or_else(|| CredentialError::UnknownProvider {
                name: declaration.name.clone(),
                provider: declaration.provider.clone(),
            })
    }
}
