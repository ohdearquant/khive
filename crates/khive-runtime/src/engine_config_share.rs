//! The `[share]` table (ADR-220 Amendment 1): the directory a caller may share
//! files from, the bounds of one share, and the computers a share can go to.
//!
//! Like `[computer]`, the table is parsed and validated with the rest of the
//! configuration file and nothing here reads from disk: a target's key is read
//! from its `key_file` only when a share to that target runs, and `root` is
//! canonicalized and checked as a directory at the same moment.

use std::path::PathBuf;

use serde::Deserialize;

use super::computer::{parse_computer_url, validate_name};
use super::{
    ComputerEndpoint, ConfigError, DEFAULT_COMPUTER_CALL_TIMEOUT_S, MAX_COMPUTER_CALL_TIMEOUT_S,
};

/// The default and the largest `max_file_bytes`: a computer's limit for one
/// blob, 64 MiB.
pub const MAX_SHARE_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// `max_total_bytes` when the table does not set it.
pub const DEFAULT_SHARE_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// The largest accepted `max_total_bytes`.
pub const MAX_SHARE_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

/// `max_files` when the table does not set it.
pub const DEFAULT_SHARE_FILES: usize = 256;

/// The largest accepted `max_files`: the most files a computer accepts in one
/// share.
pub const MAX_SHARE_FILES: usize = 1000;

/// `[share]`: what a caller may share and where it may send it.
///
/// ```toml
/// [share]
/// root = "/home/example/share"
/// max_file_bytes = 67108864
/// max_total_bytes = 268435456
/// max_files = 256
///
/// [[share.targets]]
/// name = "build-1"
/// url = "https://build-1.example/mcp"
/// key_file = "/home/example/.secrets/share-build-1.key"
/// call_timeout_s = 600
/// ```
///
/// The table and each target are closed. No value in them is a secret.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShareSectionConfig {
    /// Absolute path of the directory whose files may be shared. Everything
    /// under it can be shared, and nothing else.
    pub root: PathBuf,
    /// The largest file a share sends, in bytes.
    #[serde(default = "default_max_file_bytes")]
    pub max_file_bytes: u64,
    /// The most bytes one share sends, over all its files.
    #[serde(default = "default_max_total_bytes")]
    pub max_total_bytes: u64,
    /// The most files one share names.
    #[serde(default = "default_max_files")]
    pub max_files: usize,
    /// The computers a share can go to.
    #[serde(default)]
    pub targets: Vec<ShareTargetConfig>,
}

/// One `[[share.targets]]` entry: a computer a share can go to, reached as
/// the `[computer]` table reaches its computer.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShareTargetConfig {
    /// Names the target in a share's `target` argument and in refusals.
    pub name: String,
    /// Where the target serves MCP over streamable HTTP.
    pub url: String,
    /// Absolute path of the file holding the key the target's owner issued to
    /// this caller.
    pub key_file: PathBuf,
    /// How long one call to the target may take, in seconds.
    #[serde(default = "default_call_timeout_s")]
    pub call_timeout_s: u64,
}

fn default_max_file_bytes() -> u64 {
    MAX_SHARE_FILE_BYTES
}

fn default_max_total_bytes() -> u64 {
    DEFAULT_SHARE_TOTAL_BYTES
}

fn default_max_files() -> usize {
    DEFAULT_SHARE_FILES
}

fn default_call_timeout_s() -> u64 {
    DEFAULT_COMPUTER_CALL_TIMEOUT_S
}

fn invalid(key: &str, reason: impl Into<String>) -> ConfigError {
    ConfigError::InvalidShareConfig {
        key: key.to_owned(),
        reason: reason.into(),
    }
}

fn within(key: &str, value: u64, max: u64) -> Result<(), ConfigError> {
    if value == 0 || value > max {
        return Err(invalid(
            key,
            format!("must be between 1 and {max}; got {value}"),
        ));
    }
    Ok(())
}

impl ShareSectionConfig {
    /// Check every rule of the table. Reads nothing from disk.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.root.is_absolute() {
            return Err(invalid("root", "is not an absolute path"));
        }
        within("max_file_bytes", self.max_file_bytes, MAX_SHARE_FILE_BYTES)?;
        within(
            "max_total_bytes",
            self.max_total_bytes,
            MAX_SHARE_TOTAL_BYTES,
        )?;
        within("max_files", self.max_files as u64, MAX_SHARE_FILES as u64)?;
        if self.targets.is_empty() {
            return Err(invalid(
                "targets",
                "names no target; a [share] table needs at least one [[share.targets]] entry",
            ));
        }
        for (index, target) in self.targets.iter().enumerate() {
            target.validate(index)?;
            if self.targets[..index]
                .iter()
                .any(|earlier| earlier.name == target.name)
            {
                return Err(invalid(
                    &format!("targets[{index}].name"),
                    format!(
                        "{:?} names an earlier target too; target names are distinct",
                        target.name
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The target named `name`.
    pub fn target(&self, name: &str) -> Option<&ShareTargetConfig> {
        self.targets.iter().find(|target| target.name == name)
    }
}

impl ShareTargetConfig {
    /// The rules section 2 and section 3 of ADR-220 give the `[computer]` keys
    /// of the same names. A refusal names the target by its index.
    fn validate(&self, index: usize) -> Result<(), ConfigError> {
        let relabel = |error: ConfigError| match error {
            ConfigError::InvalidComputerConfig { key, reason } => {
                invalid(&format!("targets[{index}].{key}"), reason)
            }
            other => other,
        };
        validate_name(&self.name).map_err(relabel)?;
        self.endpoint().map_err(relabel)?;
        if !self.key_file.is_absolute() {
            return Err(invalid(
                &format!("targets[{index}].key_file"),
                "is not an absolute path",
            ));
        }
        if self.call_timeout_s == 0 || self.call_timeout_s > MAX_COMPUTER_CALL_TIMEOUT_S {
            return Err(invalid(
                &format!("targets[{index}].call_timeout_s"),
                format!(
                    "must be between 1 and {MAX_COMPUTER_CALL_TIMEOUT_S} seconds; got {}",
                    self.call_timeout_s
                ),
            ));
        }
        Ok(())
    }

    /// Parse `url` into the endpoint the client connects to, by the rules of
    /// `[computer].url`.
    pub fn endpoint(&self) -> Result<ComputerEndpoint, ConfigError> {
        parse_computer_url(&self.url)
    }
}

#[cfg(test)]
#[path = "engine_config_share_tests.rs"]
mod tests;
