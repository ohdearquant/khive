//! Compile-time identity for the source and build that produced this runtime.

/// Package version shared by runtime diagnostics and lightweight identity probes.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

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

    /// A `rerun-if-changed` path that does not exist is not "unchanged" to cargo, it is
    /// permanently stale, so the unit rebuilds on every invocation forever. `git rev-parse
    /// --git-path refs/heads/<branch>` answers with the loose ref path whether or not the ref is
    /// loose, and packing prunes that file.
    #[test]
    fn git_rerun_inputs_drop_the_loose_ref_path_once_the_ref_is_packed() {
        let repo = tempfile::tempdir().unwrap();
        init_repo_with_one_commit(repo.path());
        run_git(repo.path(), &["pack-refs", "--all"]);

        let branch = build_info_support::git_output(repo.path(), &["symbolic-ref", "-q", "HEAD"])
            .expect("a fresh repository is on a branch");
        let loose = repo.path().join(".git").join(&branch);

        // THE PRECONDITION IS ASSERTED, NOT ASSUMED. If a git version stopped pruning the
        // loose ref here the arm would otherwise pass while testing nothing.
        assert!(
            !loose.exists(),
            "packing did not prune {loose:?}; this arm no longer reproduces the condition"
        );

        let inputs = build_info_support::git_rerun_inputs(repo.path());
        let missing: Vec<_> = inputs.iter().filter(|path| !path.exists()).collect();
        assert!(
            missing.is_empty(),
            "these registered paths do not exist and would make the unit permanently stale: \
             {missing:?}"
        );
        assert!(!inputs.contains(&loose));

        // And the packed case is still observed, which is what makes dropping the loose entry
        // free rather than a loss of signal. Without this a fix that returned an empty vector
        // would satisfy the arm above.
        assert!(
            inputs.iter().any(|path| path.ends_with("packed-refs")),
            "packed-refs must stay registered: {inputs:?}"
        );
    }

    #[test]
    fn git_rerun_inputs_keep_the_loose_ref_path_while_it_exists() {
        let repo = tempfile::tempdir().unwrap();
        init_repo_with_one_commit(repo.path());

        let branch = build_info_support::git_output(repo.path(), &["symbolic-ref", "-q", "HEAD"])
            .expect("a fresh repository is on a branch");
        let loose = repo.path().join(".git").join(&branch);
        assert!(loose.exists(), "an unpacked ref must be a file: {loose:?}");

        let inputs = build_info_support::git_rerun_inputs(repo.path());
        assert!(
            inputs.contains(&loose),
            "the branch ref must stay registered while it exists: {inputs:?}"
        );
    }

    fn init_repo_with_one_commit(repo: &Path) {
        run_git(repo, &["init", "--quiet"]);
        run_git(repo, &["config", "user.email", "test@example.com"]);
        run_git(repo, &["config", "user.name", "khive test"]);
        run_git(repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("tracked.txt"), "clean\n").unwrap();
        run_git(repo, &["add", "tracked.txt"]);
        run_git(repo, &["commit", "--quiet", "-m", "baseline"]);
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
