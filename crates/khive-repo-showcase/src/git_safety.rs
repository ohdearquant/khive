//! Git command policy shared by the showcase exporter and its CLI orchestrator.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::process::Command;

const HARDENING: &[&str] = &[
    "core.hooksPath=/dev/null",
    "gc.auto=0",
    "maintenance.auto=false",
    "core.fsmonitor=false",
    "commit.gpgsign=false",
    "credential.helper=",
    "core.sshCommand=/usr/bin/false",
    "log.showSignature=false",
    "merge.verifySignatures=false",
    "gpg.program=/usr/bin/false",
    "gpg.openpgp.program=/usr/bin/false",
    "gpg.x509.program=/usr/bin/false",
    "gpg.ssh.program=/usr/bin/false",
];

/// Build the base command for clone and config inspection. Do not disable the
/// file protocol: `repo build` must be able to clone its local source cache.
pub fn hardened_git_command() -> Command {
    let mut command = Command::new("git");
    for setting in HARDENING {
        command.args(["-c", *setting]);
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE");
    command
}

/// Override every locally named content-filter driver before reading a worktree.
/// `git status` may invoke a configured clean/process program while hashing a
/// changed file, so a fixed list of Git config keys alone is insufficient.
pub fn hardened_git_command_for_repo(repo: &Path) -> io::Result<Command> {
    let output = hardened_git_command()
        .arg("-C")
        .arg(repo)
        .args(["config", "-z", "--name-only", "--get-regexp", "^filter\\."])
        .output()?;
    // Git exits 1 for an empty regex result; every other failure is a refusal
    // to run the later worktree command with potentially active filters.
    let config_read_ok =
        output.status.success() || (output.status.code() == Some(1) && output.stdout.is_empty());
    if !config_read_ok {
        return Err(io::Error::other(format!(
            "inspect repository content filters: git config exited {}",
            output.status
        )));
    }
    let names = String::from_utf8(output.stdout)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 Git filter name"))?;
    let mut drivers = BTreeSet::new();
    for name in names.split_terminator('\0') {
        let Some(rest) = name.strip_prefix("filter.") else {
            continue;
        };
        let Some((driver, _)) = rest.rsplit_once('.') else {
            continue;
        };
        drivers.insert(driver.to_owned());
    }

    let mut command = hardened_git_command();
    let mut config_count = 0usize;
    for driver in drivers {
        for (key, value) in [
            ("clean", ""),
            ("smudge", ""),
            ("process", ""),
            ("required", "false"),
        ] {
            // A driver may contain '='; `-c key=value` would split the key early.
            command
                .env(
                    format!("GIT_CONFIG_KEY_{config_count}"),
                    format!("filter.{driver}.{key}"),
                )
                .env(format!("GIT_CONFIG_VALUE_{config_count}"), value);
            config_count += 1;
        }
    }
    if config_count != 0 {
        command.env("GIT_CONFIG_COUNT", config_count.to_string());
    }
    Ok(command)
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Output;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    const AMBIENT_GIT_VARS: [&str; 7] = [
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ];
    const STATUS_ARGS: [&str; 4] = [
        "status",
        "--porcelain=v1",
        "--untracked-files=all",
        "--ignore-submodules=all",
    ];
    const SCRUB_PROBE_REPO: &str = "SHOWCASE_SCRUB_PROBE_REPO";
    const SCRUB_PROBE_OUT: &str = "SHOWCASE_SCRUB_PROBE_OUT";
    const SCRUB_PROBE_ARGS: &str = "SHOWCASE_SCRUB_PROBE_ARGS";

    /// Unhardened git with a fixed, isolated base environment plus `ambient`.
    fn plain_command(repo: &Path, args: &[&str], ambient: &[(&str, &str)]) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_OPTIONAL_LOCKS", "0");
        for var in AMBIENT_GIT_VARS {
            command.env_remove(var);
        }
        command.envs(ambient.iter().copied());
        command
    }

    fn git_ok(repo: &Path, args: &[&str]) {
        let output = plain_command(repo, args, &[]).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A committed repository whose `a.txt` is selected by `attributes`.
    fn init_repo(repo: &Path, attributes: &str) {
        fs::create_dir_all(repo).unwrap();
        git_ok(repo, &["init", "-q"]);
        fs::write(repo.join(".gitattributes"), attributes).unwrap();
        fs::write(repo.join("a.txt"), b"original\n").unwrap();
        git_ok(repo, &["add", ".gitattributes", "a.txt"]);
        git_ok(
            repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "base",
            ],
        );
    }

    /// Executable that records its own invocation; returns the shell-quoted
    /// command string and the marker path.
    fn marker_helper(dir: &Path, label: &str) -> (String, PathBuf) {
        let helper = dir.join(format!("{label}-clean.sh"));
        let marker = dir.join(format!("{label}-clean.sh.marker"));
        fs::write(&helper, "#!/bin/sh\n: > \"$0.marker\"\ncat\n").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let command = format!("'{}'", helper.display().to_string().replace('\'', "'\\''"));
        (command, marker)
    }

    /// Rewrite `a.txt` with same-size content and a stale mtime. Git compares
    /// size and mtime before hashing, so only this shape forces it to read the
    /// file through its content filter.
    fn dirty_same_size(repo: &Path, contents: &[u8]) {
        assert_eq!(contents.len(), b"original\n".len());
        let path = repo.join("a.txt");
        fs::write(&path, contents).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(946_684_800))
            .unwrap();
    }

    fn observation(output: &Output) -> String {
        format!(
            "{:?}\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout)
        )
    }

    /// Runs the hardened command inside a re-invoked copy of this test binary
    /// whose process environment carries `ambient`, so the scrub is exercised
    /// against inherited variables without touching this process's environment.
    fn hardened_in_polluted_process(
        scratch: &Path,
        repo: &Path,
        args: &[&str],
        ambient: &[(&str, &str)],
    ) -> String {
        let out = scratch.join("hardened-probe.out");
        if out.exists() {
            fs::remove_file(&out).unwrap();
        }
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "git_safety::tests::scrub_probe_child",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(SCRUB_PROBE_REPO, repo)
            .env(SCRUB_PROBE_OUT, &out)
            .env(SCRUB_PROBE_ARGS, args.join("\n"));
        for var in AMBIENT_GIT_VARS {
            child.env_remove(var);
        }
        child.envs(ambient.iter().copied());
        let result = child.output().unwrap();
        assert!(
            result.status.success(),
            "probe child failed: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        fs::read_to_string(&out).unwrap_or_else(|error| {
            panic!(
                "probe child wrote no observation ({error}): {}",
                String::from_utf8_lossy(&result.stdout)
            )
        })
    }

    /// Runs the base hardened command for the parent test's polluted
    /// environment; does nothing when invoked directly. The base command is
    /// used because the per-repository variant would also discover and
    /// override a driver named only by an inherited variable.
    #[test]
    fn scrub_probe_child() {
        let Some(repo) = std::env::var_os(SCRUB_PROBE_REPO) else {
            return;
        };
        let out = std::env::var_os(SCRUB_PROBE_OUT).unwrap();
        let args = std::env::var(SCRUB_PROBE_ARGS).unwrap();
        let output = hardened_git_command()
            .arg("-C")
            .arg(&repo)
            .args(args.lines())
            .output()
            .unwrap();
        fs::write(out, observation(&output)).unwrap();
    }

    #[test]
    fn equal_sign_and_ordinary_filter_drivers_are_neutralized() {
        let fixture = tempfile::tempdir().unwrap();
        let repo = fixture.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_OPTIONAL_LOCKS", "0")
                .env_remove("GIT_CONFIG")
                .env_remove("GIT_CONFIG_PARAMETERS")
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };

        git(&["init", "-q"]);
        fs::write(
            repo.join(".gitattributes"),
            "special.txt filter=evil=driver\nordinary.txt filter=plain\n",
        )
        .unwrap();
        for file in ["special.txt", "ordinary.txt"] {
            fs::write(repo.join(file), b"original\n").unwrap();
        }
        git(&["add", ".gitattributes", "special.txt", "ordinary.txt"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "base",
        ]);

        let mut markers = Vec::new();
        for (driver, label) in [("evil=driver", "special"), ("plain", "ordinary")] {
            let helper = fixture.path().join(format!("{label}-clean.sh"));
            let marker = fixture.path().join(format!("{label}-clean.sh.marker"));
            fs::write(&helper, "#!/bin/sh\n: > \"$0.marker\"\ncat\n").unwrap();
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
            let helper_command =
                format!("'{}'", helper.display().to_string().replace('\'', "'\\''"));
            git(&["config", &format!("filter.{driver}.clean"), &helper_command]);
            git(&["config", &format!("filter.{driver}.required"), "true"]);
            markers.push(marker);
        }

        let make_dirty = |contents: &[u8]| {
            for file in ["special.txt", "ordinary.txt"] {
                let path = repo.join(file);
                fs::write(&path, contents).unwrap();
                OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(UNIX_EPOCH + Duration::from_secs(946_684_800))
                    .unwrap();
            }
        };
        make_dirty(b"changed!\n");
        let control = git(&[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=all",
        ]);
        assert!(
            !control.stdout.is_empty(),
            "parent repository must be dirty"
        );
        for marker in &markers {
            assert!(marker.is_file(), "ordinary git status must run {marker:?}");
            fs::remove_file(marker).unwrap();
        }
        make_dirty(b"updated!\n");

        let status = hardened_git_command_for_repo(&repo)
            .unwrap()
            .arg("-C")
            .arg(&repo)
            .args([
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignore-submodules=all",
            ])
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "hardened status: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(
            !status.stdout.is_empty(),
            "modified files must remain visible"
        );
        for marker in &markers {
            assert!(!marker.exists(), "hardened status ran {marker:?}");
        }

        for driver in ["evil=driver", "plain"] {
            for (key, expected) in [
                ("clean", ""),
                ("smudge", ""),
                ("process", ""),
                ("required", "false"),
            ] {
                let name = format!("filter.{driver}.{key}");
                let effective = hardened_git_command_for_repo(&repo)
                    .unwrap()
                    .arg("-C")
                    .arg(&repo)
                    .args(["config", "--get", &name])
                    .output()
                    .unwrap();
                assert!(effective.status.success(), "{name}");
                assert_eq!(
                    String::from_utf8(effective.stdout).unwrap().trim(),
                    expected,
                    "{name} must be overridden before status"
                );
            }
        }
    }

    #[test]
    fn empty_named_filter_driver_is_neutralized() {
        let fixture = tempfile::tempdir().unwrap();
        let repo = fixture.path().join("repo");
        init_repo(&repo, "a.txt filter=\n");
        let (helper, marker) = marker_helper(fixture.path(), "empty-driver");
        let mut config = OpenOptions::new()
            .append(true)
            .open(repo.join(".git/config"))
            .unwrap();
        write!(
            config,
            "[filter \"\"]\n\tclean = {helper}\n\trequired = true\n"
        )
        .unwrap();
        drop(config);

        dirty_same_size(&repo, b"changed!\n");
        let control = plain_command(&repo, &STATUS_ARGS, &[]).output().unwrap();
        assert!(!control.stdout.is_empty(), "fixture must be dirty");
        assert!(
            marker.is_file(),
            "ordinary git status must run the empty-named driver"
        );
        fs::remove_file(&marker).unwrap();
        dirty_same_size(&repo, b"updated!\n");

        let status = hardened_git_command_for_repo(&repo)
            .unwrap()
            .arg("-C")
            .arg(&repo)
            .args(STATUS_ARGS)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "hardened status: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(
            !status.stdout.is_empty(),
            "modified file must remain visible"
        );
        assert!(
            !marker.exists(),
            "hardened status ran the empty-named driver"
        );
    }

    #[test]
    fn signature_settings_are_overridden_over_repository_config() {
        let fixture = tempfile::tempdir().unwrap();
        let repo = fixture.path().join("repo");
        init_repo(&repo, "");
        for key in [
            "log.showSignature",
            "gpg.program",
            "gpg.openpgp.program",
            "gpg.x509.program",
            "gpg.ssh.program",
        ] {
            git_ok(&repo, &["config", key, "hostile"]);
        }
        for (key, expected) in [
            ("log.showSignature", "false"),
            ("gpg.program", "/usr/bin/false"),
            ("gpg.openpgp.program", "/usr/bin/false"),
            ("gpg.x509.program", "/usr/bin/false"),
            ("gpg.ssh.program", "/usr/bin/false"),
        ] {
            let effective = hardened_git_command_for_repo(&repo)
                .unwrap()
                .arg("-C")
                .arg(&repo)
                .args(["config", "--get", key])
                .output()
                .unwrap();
            assert!(effective.status.success(), "{key}");
            assert_eq!(
                String::from_utf8(effective.stdout).unwrap().trim(),
                expected,
                "{key} must be overridden over repository configuration"
            );
        }
    }

    #[test]
    fn inherited_git_environment_does_not_reach_the_hardened_command() {
        let fixture = tempfile::tempdir().unwrap();
        let scratch = fixture.path();

        // Configuration variables that name a content filter: the marker shows
        // whether the filter ran during a status that must rehash `a.txt`.
        let (helper, marker) = marker_helper(scratch, "ambient");
        let helper_path = helper.trim_matches('\'').to_owned();
        let parameters = format!("'filter.probe.clean={helper_path}'");
        let filter_cases: [(&str, Vec<(&str, &str)>); 2] = [
            (
                "GIT_CONFIG_PARAMETERS",
                vec![("GIT_CONFIG_PARAMETERS", parameters.as_str())],
            ),
            (
                "GIT_CONFIG_COUNT",
                vec![
                    ("GIT_CONFIG_COUNT", "1"),
                    ("GIT_CONFIG_KEY_0", "filter.probe.clean"),
                    ("GIT_CONFIG_VALUE_0", helper_path.as_str()),
                ],
            ),
        ];
        for (label, ambient) in &filter_cases {
            let repo = scratch.join(label);
            init_repo(&repo, "a.txt filter=probe\n");
            dirty_same_size(&repo, b"changed!\n");
            let control = plain_command(&repo, &STATUS_ARGS, ambient)
                .output()
                .unwrap();
            assert!(!control.stdout.is_empty(), "{label}: fixture must be dirty");
            assert!(
                marker.is_file(),
                "{label}: plain git must run the inherited filter"
            );
            fs::remove_file(&marker).unwrap();
            dirty_same_size(&repo, b"updated!\n");

            let hardened = hardened_in_polluted_process(scratch, &repo, &STATUS_ARGS, ambient);
            assert!(
                hardened.contains("a.txt"),
                "{label}: modified file must remain visible: {hardened}"
            );
            assert!(
                !marker.exists(),
                "{label}: inherited variable reached the hardened command"
            );
        }

        // Variables that redirect which repository, index or configuration
        // git reads: the observation must equal the unpolluted one.
        let repo = scratch.join("query");
        init_repo(&repo, "");
        let other = scratch.join("other");
        init_repo(&other, "");
        let other_git = other.join(".git");
        let config_file = scratch.join("inherited.cfg");
        fs::write(&config_file, "[probe]\n\tkey = inherited\n").unwrap();
        let index_file = scratch.join("inherited-index");
        let query_cases: [(&str, String, &[&str]); 5] = [
            (
                "GIT_DIR",
                other_git.display().to_string(),
                &["rev-parse", "--git-dir"],
            ),
            (
                "GIT_WORK_TREE",
                other.display().to_string(),
                &["rev-parse", "--show-toplevel"],
            ),
            (
                "GIT_COMMON_DIR",
                other_git.display().to_string(),
                &["rev-parse", "--git-common-dir"],
            ),
            (
                "GIT_INDEX_FILE",
                index_file.display().to_string(),
                &["rev-parse", "--git-path", "index"],
            ),
            (
                "GIT_CONFIG",
                config_file.display().to_string(),
                &["config", "--get", "probe.key"],
            ),
        ];
        for (var, value, args) in &query_cases {
            let ambient = [(*var, value.as_str())];
            let baseline = observation(&plain_command(&repo, args, &[]).output().unwrap());
            let control = observation(&plain_command(&repo, args, &ambient).output().unwrap());
            assert_ne!(
                control, baseline,
                "{var}: plain git must observe the variable"
            );
            let hardened = hardened_in_polluted_process(scratch, &repo, args, &ambient);
            assert_eq!(hardened, baseline, "{var} reached the hardened command");
        }
    }
}
