use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::engine_config::ConfigError;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MountEffect {
    Read,
    #[default]
    Mutating,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MountToolConfig {
    pub name: String,
    #[serde(default)]
    pub effect: MountEffect,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MountConfig {
    pub name: String,
    pub transport: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    pub credential: Option<String>,
    #[serde(default)]
    pub tools: Vec<MountToolConfig>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    30_000
}

fn environment_name(name: &str) -> bool {
    let mut chars = name.bytes();
    matches!(chars.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && chars.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

pub fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

pub fn validate_mounts(mounts: &[MountConfig]) -> Result<(), ConfigError> {
    let refuse = |reason: &str| ConfigError::InvalidMountConfig {
        reason: reason.into(),
    };
    let mut names = BTreeSet::new();
    for mount in mounts {
        if mount.tools.len() > 1024 {
            return Err(refuse("a mount may configure at most 1024 tools"));
        }
        if mount.name.is_empty()
            || mount.name.len() > 128
            || !mount
                .name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            return Err(refuse(
                "name must contain lowercase ASCII letters, digits or hyphens",
            ));
        }
        if !names.insert(&mount.name) {
            return Err(refuse("duplicate mount name"));
        }
        if mount.transport != "stdio" {
            return Err(refuse("only stdio transport is supported"));
        }
        if mount.command.is_empty()
            || mount.command.contains('\0')
            || mount.args.iter().any(|arg| arg.contains('\0'))
        {
            return Err(refuse(
                "command must be non-empty; command and args must contain no U+0000",
            ));
        }
        if mount
            .credential
            .as_deref()
            .is_some_and(|name| !environment_name(name))
        {
            return Err(refuse(
                "credential must be an environment-variable name, never an inline value",
            ));
        }
        if mount.env.iter().any(|name| !environment_name(name)) {
            return Err(refuse("env must contain environment-variable names only"));
        }
        if mount.timeout_ms == 0 {
            return Err(refuse("timeout_ms must be positive"));
        }
        let mut tools = BTreeSet::new();
        for tool in &mount.tools {
            if !valid_tool_name(&tool.name) {
                return Err(refuse(
                    "tool name must contain ASCII letters, digits, underscores or hyphens",
                ));
            }
            if !tools.insert(&tool.name) {
                return Err(refuse("duplicate tool name"));
            }
        }
    }
    Ok(())
}
