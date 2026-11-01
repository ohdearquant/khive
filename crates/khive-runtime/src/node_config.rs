//! The `[channel]` section of the khive config file.
//!
//! A `[channel.node]` table is still accepted so an existing config file keeps
//! loading. Its contents are held as an opaque value and are not interpreted.

use serde::{Deserialize, Serialize};

use crate::engine_config::ConfigError;

/// The `[channel]` section of the khive config file.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelSectionConfig {
    /// The `[channel.node]` table, when present. It is accepted as an opaque
    /// value and nothing reads it.
    #[serde(default)]
    pub node: Option<toml::Value>,
}

impl ChannelSectionConfig {
    /// Validate the section. A section that has parsed is always valid.
    pub fn validate(&self) -> Result<(), ConfigError> {
        Ok(())
    }
}
