use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn source_revision(repo_root: &Path) -> Option<String> {
    let revision = git_output(repo_root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let status = git_command(repo_root)
        .args(["status", "--porcelain=v1", "--untracked-files=no"])
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }

    if status.stdout.is_empty() {
        Some(revision)
    } else {
        Some(format!("{revision}-dirty"))
    }
}

/// The git files whose changes must invalidate the build stamp, as paths that EXIST.
///
/// A `rerun-if-changed` path that does not exist is not "unchanged" to cargo, it is
/// permanently stale: the unit is rebuilt on every invocation, forever, and so is
/// everything above it. `git rev-parse --git-path refs/heads/<branch>` answers with the
/// LOOSE ref path whether or not that ref is loose, so on a repository whose refs have been
/// packed -- the ordinary state after a `git gc` and in a fresh clone -- the answer names
/// nothing on disk.
///
/// Dropping an absent path costs no signal here, and it is worth saying why rather than
/// leaving it to be rediscovered: `packed-refs` is one of the inputs below and it does
/// exist, so a packed ref that moves still invalidates the stamp through that file. The
/// loose entry is redundant in exactly the case where it is missing.
pub(crate) fn git_rerun_inputs(repo_root: &Path) -> Vec<PathBuf> {
    let mut names = vec![
        "HEAD".to_string(),
        "index".to_string(),
        "packed-refs".to_string(),
    ];
    if let Some(reference) = git_output(repo_root, &["symbolic-ref", "-q", "HEAD"]) {
        names.push(reference);
    }

    let mut paths = Vec::with_capacity(names.len());
    for name in names {
        let Some(path) = git_output(repo_root, &["rev-parse", "--git-path", &name]) else {
            continue;
        };
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            repo_root.join(path)
        };
        if !path.exists() {
            continue;
        }
        paths.push(path);
    }
    paths
}

pub(crate) fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = git_command(cwd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(cwd);
    command
}
