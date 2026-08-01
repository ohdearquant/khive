//! Compile-time identity for the source and build that produced this runtime.

#[cfg(test)]
#[path = "build_info_support.rs"]
mod build_info_support;

/// Explicit source identity used when the build cannot inspect a Git checkout.
pub const UNSTAMPED_REVISION: &str = "unstamped";

/// Explicit build-time fallback used when no compile-time timestamp is available.
pub const UNKNOWN_BUILD_TIME: &str = "unknown";

/// Immutable provenance stamped into the binary at compile time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildInfo {
    pub source_revision: &'static str,
    pub build_time: &'static str,
}

impl BuildInfo {
    /// Construct build information while preserving an explicit unstamped state.
    pub const fn new(
        source_revision: Option<&'static str>,
        build_time: Option<&'static str>,
    ) -> Self {
        Self {
            source_revision: match source_revision {
                Some(revision) => revision,
                None => UNSTAMPED_REVISION,
            },
            build_time: match build_time {
                Some(build_time) => build_time,
                None => UNKNOWN_BUILD_TIME,
            },
        }
    }

    pub fn is_stamped(&self) -> bool {
        self.source_revision != UNSTAMPED_REVISION
    }
}

/// Provenance for the currently compiled runtime.
pub const BUILD_INFO: BuildInfo = BuildInfo::new(
    option_env!("KHIVE_SOURCE_REVISION"),
    option_env!("KHIVE_BUILD_TIME"),
);

/// Rich version string used by `kkernel --version`.
pub const BUILD_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (revision ",
    env!("KHIVE_SOURCE_REVISION"),
    ", built ",
    env!("KHIVE_BUILD_TIME"),
    ")"
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn stamped_build_preserves_full_dirty_revision_and_time() {
        let info = BuildInfo::new(
            Some("45131c27b615f641c579046513d7c0ddd15c0bfb-dirty"),
            Some("2026-07-31T20:00:00Z"),
        );

        assert_eq!(
            info.source_revision,
            "45131c27b615f641c579046513d7c0ddd15c0bfb-dirty"
        );
        assert_eq!(info.build_time, "2026-07-31T20:00:00Z");
        assert!(info.is_stamped());
    }

    #[test]
    fn unstamped_build_is_explicit() {
        let info = BuildInfo::new(None, None);

        assert_eq!(info.source_revision, UNSTAMPED_REVISION);
        assert_eq!(info.build_time, UNKNOWN_BUILD_TIME);
        assert!(!info.is_stamped());
    }

    #[test]
    fn compiled_version_uses_the_compiled_provenance() {
        assert!(BUILD_VERSION.contains(BUILD_INFO.source_revision));
        assert!(BUILD_VERSION.contains(BUILD_INFO.build_time));

        if BUILD_INFO.is_stamped() {
            let revision = BUILD_INFO
                .source_revision
                .strip_suffix("-dirty")
                .unwrap_or(BUILD_INFO.source_revision);
            assert_eq!(revision.len(), 40);
            assert!(revision.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn revision_derivation_distinguishes_clean_dirty_and_unstamped() {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "--quiet"]);
        run_git(repo.path(), &["config", "user.email", "test@example.com"]);
        run_git(repo.path(), &["config", "user.name", "khive test"]);
        run_git(repo.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.path().join("tracked.txt"), "clean\n").unwrap();
        run_git(repo.path(), &["add", "tracked.txt"]);
        run_git(repo.path(), &["commit", "--quiet", "-m", "baseline"]);

        let revision = build_info_support::git_output(repo.path(), &["rev-parse", "HEAD"])
            .expect("temp repository must have a revision");
        assert_eq!(
            build_info_support::source_revision(repo.path()).as_deref(),
            Some(revision.as_str())
        );

        std::fs::write(repo.path().join("untracked.txt"), "not compiled\n").unwrap();
        assert_eq!(
            build_info_support::source_revision(repo.path()).as_deref(),
            Some(revision.as_str())
        );

        std::fs::write(repo.path().join("tracked.txt"), "dirty\n").unwrap();
        assert_eq!(
            build_info_support::source_revision(repo.path()),
            Some(format!("{revision}-dirty"))
        );

        let non_git = tempfile::tempdir().unwrap();
        assert_eq!(build_info_support::source_revision(non_git.path()), None);
    }

    fn run_git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git must be available for build-provenance tests");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
