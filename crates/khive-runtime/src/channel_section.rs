//! Opaque channel configuration and instance-bound component validation.

use std::collections::{btree_map::Entry, BTreeMap};
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::engine_config::ConfigError;
use crate::RuntimeError;

/// Values under `[channel]`, keyed by the component's exact registered name.
///
/// Unknown names are retained without effect. Only a registered component
/// interprets its value; a value alone never starts a component or installs a route.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct ChannelSectionConfig {
    values: BTreeMap<String, toml::Value>,
}

impl ChannelSectionConfig {
    pub fn get(&self, name: &str) -> Option<&toml::Value> {
        self.values.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &toml::Value)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value))
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn insert(&mut self, name: String, value: toml::Value) -> Option<toml::Value> {
        self.values.insert(name, value)
    }

    /// Loading preserves every value; component validation happens at activation.
    pub fn validate(&self) -> Result<(), ConfigError> {
        Ok(())
    }
}

impl From<BTreeMap<String, toml::Value>> for ChannelSectionConfig {
    fn from(values: BTreeMap<String, toml::Value>) -> Self {
        Self { values }
    }
}

/// A component validates its own exact value, or its absence, before activation.
/// Checks must be pure and idempotent: host preparation and pack activation may
/// both invoke them. They must not start work or depend on exactly one invocation.
pub type ChannelConfigValidator =
    Arc<dyn Fn(Option<&toml::Value>) -> Result<(), RuntimeError> + Send + Sync>;

/// Validators supplied by the host before constructing its pack registry.
///
/// Clones retain the same callbacks. Independent registries share no mutable
/// registration state; callbacks must not start resident work during validation.
#[derive(Clone, Default)]
pub struct ChannelConfigValidators {
    validators: BTreeMap<String, ChannelConfigValidator>,
}

impl fmt::Debug for ChannelConfigValidators {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChannelConfigValidators")
            .field("names", &self.validators.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ChannelConfigValidators {
    /// Register one component name without replacing an existing callback.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        validator: ChannelConfigValidator,
    ) -> Result<(), RuntimeError> {
        let name = name.into();
        if name.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "channel component name must not be empty".into(),
            ));
        }
        match self.validators.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(validator);
                Ok(())
            }
            Entry::Occupied(entry) => Err(RuntimeError::InvalidInput(format!(
                "channel component {:?} is already registered",
                entry.key()
            ))),
        }
    }

    /// Validate registered components in name order, preserving their typed errors.
    /// Unregistered configuration remains retained and inert.
    pub fn validate(&self, section: &ChannelSectionConfig) -> Result<(), RuntimeError> {
        for (name, validator) in &self.validators {
            validator(section.get(name))?;
        }
        Ok(())
    }
}
