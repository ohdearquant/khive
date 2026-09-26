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
    for driver in drivers {
        for key in ["clean", "smudge", "process"] {
            command.arg("-c").arg(format!("filter.{driver}.{key}="));
        }
        command
            .arg("-c")
            .arg(format!("filter.{driver}.required=false"));
    }
    Ok(command)
}
