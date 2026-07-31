use std::path::Path;
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
        .arg("-C")
        .arg(cwd);
    command
}
