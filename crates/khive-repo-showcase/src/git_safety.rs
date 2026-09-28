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
        if !driver.is_empty() {
            drivers.insert(driver.to_owned());
        }
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
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

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
}
