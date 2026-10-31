//! Configuration for ADR-087's accepted workspace mirror scope.
//!
//! Paths are project-root-relative (for example `.khive/notes/handoffs/x.md`).
//! Lists are JSON arrays of glob strings; unset uses defaults, while an empty
//! value or `[]` replaces that list with no patterns. Exclusions win. Defaults
//! select Markdown/text under notes, reports and local review history, plus
//! Markdown workspace artifacts/completion reports. They do not adopt the
//! broader include scope in ADR-087's proposed Amendment 1.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

const POLL: &str = "KHIVE_MIRROR_WORKSPACE_POLL_SECS";
const INCLUDE: &str = "KHIVE_MIRROR_WORKSPACE_INCLUDE";
const EXCLUDE: &str = "KHIVE_MIRROR_WORKSPACE_EXCLUDE";
const MAX_FILE_BYTES: &str = "KHIVE_MIRROR_WORKSPACE_MAX_FILE_BYTES";
const DEFAULT_INCLUDE: &[&str] = &[
    ".khive/notes/**/*.{md,markdown,txt}",
    ".khive/reports/**/*.{md,markdown,txt}",
    ".khive/codex_reviews/**/*.{md,markdown,txt}",
    ".khive/workspaces/*/artifacts/**/*.{md,markdown}",
    ".khive/workspaces/*/completion-report.{md,markdown}",
];
const DEFAULT_EXCLUDE: &[&str] = &[
    ".khive/kg/**/*.ndjson",
    ".khive/kg/**/schema.yaml",
    ".khive/scripts/**",
    "**/{target,build,dist,node_modules,.cache,cache}/**",
    "**/*.{bin,exe,dll,so,dylib,o,a,wasm,zip,gz,png,jpg,jpeg,gif,pdf,db,sqlite,sqlite3}",
];

/// Invalid mirror configuration, without the contents of environment values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceMirrorConfigError {
    InvalidPollInterval,
    InvalidMaxFileBytes,
    NonUnicodeValue(&'static str),
    InvalidGlobList(&'static str),
    InvalidGlob(&'static str),
    GlobSetBuild(&'static str),
}

impl fmt::Display for WorkspaceMirrorConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPollInterval => {
                write!(f, "{POLL} must be a nonzero u64 number of seconds")
            }
            Self::InvalidMaxFileBytes => {
                write!(f, "{MAX_FILE_BYTES} must be a nonzero u64 number of bytes")
            }
            Self::NonUnicodeValue(key) => write!(f, "{key} must contain Unicode text"),
            Self::InvalidGlobList(key) => write!(f, "{key} must be a JSON array of glob strings"),
            Self::InvalidGlob(key) => write!(f, "{key} contains an invalid glob"),
            Self::GlobSetBuild(key) => write!(f, "{key} could not be compiled into a glob set"),
        }
    }
}
impl std::error::Error for WorkspaceMirrorConfigError {}

/// Compiled path selection and nonzero interval; this type starts no mirror.
///
/// Globs use `/` separators and explicit backslash escaping on every platform.
/// Default extension exclusions are path filters, not binary-content detection.
pub struct WorkspaceMirrorConfig {
    poll_interval: Duration,
    max_file_bytes: Option<u64>,
    include_patterns: Vec<String>,
    exclude_patterns: Vec<String>,
    include: GlobSet,
    exclude: GlobSet,
}

impl WorkspaceMirrorConfig {
    /// Read the workspace mirror variables. Missing interval means 2s; an unset
    /// file-size limit means unlimited. Zero, malformed and overflowing values
    /// for either numeric setting are configuration errors.
    pub fn from_env() -> Result<Self, WorkspaceMirrorConfigError> {
        Self::from_lookup(|key| match std::env::var(key) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(WorkspaceMirrorConfigError::NonUnicodeValue(key))
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn from_defaults_for_test() -> Result<Self, WorkspaceMirrorConfigError> {
        Self::from_lookup(|_| Ok(None))
    }

    fn from_lookup(
        mut lookup: impl FnMut(&'static str) -> Result<Option<String>, WorkspaceMirrorConfigError>,
    ) -> Result<Self, WorkspaceMirrorConfigError> {
        let poll_secs = match lookup(POLL)? {
            None => 2,
            Some(raw) => raw
                .parse::<u64>()
                .ok()
                .filter(|seconds| *seconds != 0)
                .ok_or(WorkspaceMirrorConfigError::InvalidPollInterval)?,
        };
        let max_file_bytes = match lookup(MAX_FILE_BYTES)? {
            None => None,
            Some(raw) => Some(
                raw.parse::<u64>()
                    .ok()
                    .filter(|bytes| *bytes != 0)
                    .ok_or(WorkspaceMirrorConfigError::InvalidMaxFileBytes)?,
            ),
        };
        let include_patterns = patterns(lookup(INCLUDE)?, DEFAULT_INCLUDE, INCLUDE)?;
        let exclude_patterns = patterns(lookup(EXCLUDE)?, DEFAULT_EXCLUDE, EXCLUDE)?;
        let include = compile(&include_patterns, INCLUDE)?;
        let exclude = compile(&exclude_patterns, EXCLUDE)?;
        Ok(Self {
            poll_interval: Duration::from_secs(poll_secs),
            max_file_bytes,
            include_patterns,
            exclude_patterns,
            include,
            exclude,
        })
    }

    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }
    /// Maximum selected-file bytes, or `None` when no limit is configured.
    pub fn max_file_bytes(&self) -> Option<u64> {
        self.max_file_bytes
    }
    #[cfg(test)]
    pub(crate) fn set_max_file_bytes_for_test(&mut self, value: Option<u64>) {
        self.max_file_bytes = value;
    }
    pub fn include_patterns(&self) -> &[String] {
        &self.include_patterns
    }
    pub fn exclude_patterns(&self) -> &[String] {
        &self.exclude_patterns
    }

    /// Match a lexical project-relative path, without reading the filesystem.
    /// Absolute/prefixed paths and parent traversal are refused; `.` is ignored.
    pub fn includes(&self, path: impl AsRef<Path>) -> bool {
        let mut relative = PathBuf::new();
        for component in path.as_ref().components() {
            match component {
                Component::Normal(part) => relative.push(part),
                Component::CurDir => {}
                _ => return false,
            }
        }
        !relative.as_os_str().is_empty()
            && self.include.is_match(&relative)
            && !self.exclude.is_match(&relative)
    }
}

fn patterns(
    raw: Option<String>,
    defaults: &[&str],
    key: &'static str,
) -> Result<Vec<String>, WorkspaceMirrorConfigError> {
    match raw {
        None => Ok(defaults
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect()),
        Some(raw) if raw.trim().is_empty() => Ok(Vec::new()),
        Some(raw) => {
            serde_json::from_str(&raw).map_err(|_| WorkspaceMirrorConfigError::InvalidGlobList(key))
        }
    }
}

fn compile(patterns: &[String], key: &'static str) -> Result<GlobSet, WorkspaceMirrorConfigError> {
    let mut set = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .map_err(|_| WorkspaceMirrorConfigError::InvalidGlob(key))?;
        set.add(glob);
    }
    set.build()
        .map_err(|_| WorkspaceMirrorConfigError::GlobSetBuild(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(
        values: &[(&str, &str)],
    ) -> Result<WorkspaceMirrorConfig, WorkspaceMirrorConfigError> {
        WorkspaceMirrorConfig::from_lookup(|key| {
            Ok(values
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned()))
        })
    }

    #[test]
    fn defaults_select_accepted_base_artifacts() {
        let config = config(&[]).expect("valid default globs");
        assert_eq!(config.poll_interval(), Duration::from_secs(2));
        for path in [
            ".khive/notes/handoffs/x.md",
            ".khive/reports/result.txt",
            ".khive/codex_reviews/pr-1/review.markdown",
            ".khive/workspaces/task-1/artifacts/report.md",
            ".khive/workspaces/task-1/completion-report.md",
        ] {
            assert!(
                config.includes(path),
                "default artifact must be selected: {path}"
            );
        }
        for path in [
            ".khive/kg/graph.ndjson",
            ".khive/kg/schema.yaml",
            ".khive/scripts/run.sh",
            ".khive/workspaces/task-1/source.rs",
            ".khive/audits/report.md",
            ".khive/loop/report.md",
        ] {
            assert!(
                !config.includes(path),
                "outside default artifact scope: {path}"
            );
        }
    }

    #[test]
    fn positive_poll_interval_is_used_as_given() {
        for seconds in [1, 17, u64::MAX] {
            let raw = seconds.to_string();
            let config = config(&[(POLL, &raw)]).expect("positive interval");
            assert_eq!(
                config.poll_interval(),
                Duration::from_secs(seconds),
                "positive poll interval must be used as given"
            );
        }
    }

    #[test]
    fn zero_malformed_and_overflow_poll_intervals_are_refused() {
        for raw in ["0", "", "-1", "not-a-number", "18446744073709551616"] {
            assert!(
                matches!(
                    config(&[(POLL, raw)]),
                    Err(WorkspaceMirrorConfigError::InvalidPollInterval)
                ),
                "zero, malformed and overflowing intervals must be refused"
            );
        }
    }

    #[test]
    fn file_size_limit_defaults_to_unlimited_and_accepts_positive_u64() {
        assert_eq!(config(&[]).expect("default config").max_file_bytes(), None);
        for bytes in [1, 4096, u64::MAX] {
            let raw = bytes.to_string();
            let config = config(&[(MAX_FILE_BYTES, &raw)]).expect("positive byte limit");
            assert_eq!(config.max_file_bytes(), Some(bytes));
        }
    }

    #[test]
    fn invalid_file_size_limits_are_refused_without_echoing_values() {
        for raw in ["0", "", "-1", "private-value", "18446744073709551616"] {
            let error = match config(&[(MAX_FILE_BYTES, raw)]) {
                Err(error) => error,
                Ok(_) => panic!("invalid file-size limit must be refused"),
            };
            assert_eq!(error, WorkspaceMirrorConfigError::InvalidMaxFileBytes);
            assert!(!error.to_string().contains("private-value"));
            assert!(!format!("{error:?}").contains("private-value"));
        }
    }

    #[test]
    fn include_override_replaces_defaults_and_keeps_exclusions() {
        let config =
            config(&[(INCLUDE, r#"[".khive/reports/**"]"#)]).expect("valid include override");
        assert!(
            !config.includes(".khive/notes/handoffs/x.md"),
            "include override must replace defaults"
        );
        assert!(config.includes(".khive/reports/report.md"));
        assert!(
            !config.includes(".khive/reports/result.bin"),
            "default binary exclusion remains active"
        );
        assert_eq!(config.include_patterns(), &[".khive/reports/**".to_owned()]);
        assert_eq!(config.exclude_patterns().len(), DEFAULT_EXCLUDE.len());
    }

    #[test]
    fn exclude_override_replaces_defaults_and_wins_matches() {
        let config = config(&[
            (INCLUDE, r#"[".khive/**"]"#),
            (EXCLUDE, r#"[".khive/notes/**"]"#),
        ])
        .expect("valid independent overrides");
        assert!(
            !config.includes(".khive/notes/handoffs/x.md"),
            "exclude wins over include"
        );
        assert!(
            config.includes(".khive/scripts/run.sh"),
            "exclude override must replace defaults"
        );
        assert!(config.includes(".khive/kg/graph.ndjson"));
        assert_eq!(config.exclude_patterns(), &[".khive/notes/**".to_owned()]);
    }

    #[test]
    fn broad_include_retains_real_default_exclusions() {
        let config = config(&[(INCLUDE, r#"[".khive/**"]"#)]).expect("broad include");
        assert!(config.includes(".khive/notes/handoffs/x.md"));
        for path in [
            ".khive/kg/graph.ndjson",
            ".khive/kg/schema.yaml",
            ".khive/scripts/run.sh",
            ".khive/notes/target/report.md",
            ".khive/reports/.cache/report.md",
            ".khive/workspaces/task-1/artifacts/image.png",
        ] {
            assert!(!config.includes(path), "default exclusion must win: {path}");
        }
    }

    #[test]
    fn explicit_empty_overrides_replace_with_empty_sets() {
        for empty in ["", " \n ", "[]"] {
            let includes = config(&[(INCLUDE, empty)]).expect("empty include set");
            assert!(includes.include_patterns().is_empty());
            assert!(!includes.includes(".khive/notes/handoffs/x.md"));
            let excludes = config(&[(INCLUDE, r#"[".khive/**"]"#), (EXCLUDE, empty)])
                .expect("empty exclude set");
            assert!(excludes.exclude_patterns().is_empty());
            assert!(excludes.includes(".khive/scripts/run.sh"));
        }
    }

    #[test]
    fn malformed_lists_and_globs_do_not_echo_supplied_values() {
        for raw in [
            r#"["private-value""#,
            r#"[17]"#,
            r#"{"private-value":true}"#,
        ] {
            let error = match config(&[(INCLUDE, raw)]) {
                Err(error) => error,
                Ok(_) => panic!("invalid list must be refused"),
            };
            assert_eq!(error, WorkspaceMirrorConfigError::InvalidGlobList(INCLUDE));
            assert!(!error.to_string().contains("private-value"));
            assert!(!format!("{error:?}").contains("private-value"));
        }
        let error = match config(&[(EXCLUDE, r#"["[private-value"]"#)]) {
            Err(error) => error,
            Ok(_) => panic!("invalid glob must be refused"),
        };
        assert_eq!(error, WorkspaceMirrorConfigError::InvalidGlob(EXCLUDE));
        assert!(!error.to_string().contains("private-value"));
        assert!(!format!("{error:?}").contains("private-value"));
    }

    #[test]
    fn matching_uses_project_relative_components_and_real_recursive_globs() {
        let config = config(&[]).expect("default config");
        assert!(config.includes("./.khive/notes/handoffs/x.md"));
        let native = PathBuf::from(".khive")
            .join("notes")
            .join("handoffs")
            .join("x.md");
        assert!(config.includes(native));
        #[cfg(unix)]
        assert!(
            !config.includes(r".khive\notes\handoffs\x.md"),
            "Unix literal backslashes are not native separators"
        );
        assert!(
            config.includes(".khive/reports/report.md"),
            "double-star matches zero directories"
        );
        assert!(config.includes(".khive/reports/nested/report.md"));
        assert!(!config.includes(".khive/notes/../reports/report.md"));
        assert!(!config.includes("../.khive/reports/report.md"));
        assert!(!config.includes("."));
        let absolute =
            PathBuf::from(std::path::MAIN_SEPARATOR.to_string()).join(".khive/reports/report.md");
        assert!(!config.includes(absolute));
        assert!(
            !config.includes(".khive/workspaces/task-1/nested/artifacts/report.md"),
            "single-star workspace slot cannot consume a separator"
        );
    }
}
