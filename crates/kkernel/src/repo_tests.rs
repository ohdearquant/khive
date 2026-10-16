use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use super::*;

#[test]
fn clean_snapshot_never_runs_repo_configured_filter() {
    let fixture = tempfile::tempdir().unwrap();
    let repo = fixture.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(repo.join(".gitattributes"), "tracked.txt filter=marker\n").unwrap();
    std::fs::write(repo.join("tracked.txt"), "original\n").unwrap();
    git(&["add", ".gitattributes", "tracked.txt"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "base",
    ]);

    let helper = fixture.path().join("fake-clean.sh");
    let marker = fixture.path().join("fake-clean.sh.marker");
    std::fs::write(&helper, "#!/bin/sh\n: > \"$0.marker\"\ncat\n").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(&["config", "filter.marker.clean", helper.to_str().unwrap()]);
    git(&["config", "filter.marker.smudge", helper.to_str().unwrap()]);
    git(&["config", "filter.marker.process", helper.to_str().unwrap()]);
    git(&["config", "filter.marker.required", "true"]);
    // Git compares size and mtime before hashing, so only a same-size edit
    // with a stale mtime forces it to read the file through the filter.
    let make_dirty = |contents: &[u8]| {
        assert_eq!(contents.len(), b"original\n".len());
        let path = repo.join("tracked.txt");
        std::fs::write(&path, contents).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(946_684_800))
            .unwrap();
    };
    make_dirty(b"changed!\n");

    let attribute = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["check-attr", "filter", "--", "tracked.txt"])
        .output()
        .unwrap();
    assert!(attribute.status.success());
    assert_eq!(
        String::from_utf8(attribute.stdout).unwrap().trim(),
        "tracked.txt: filter: marker"
    );
    let configured_clean = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["config", "--get", "filter.marker.clean"])
        .output()
        .unwrap();
    assert!(configured_clean.status.success());
    assert_eq!(
        String::from_utf8(configured_clean.stdout).unwrap().trim(),
        helper.to_str().unwrap()
    );

    Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=all",
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .unwrap();
    assert!(marker.is_file(), "ordinary git status must run the filter");
    std::fs::remove_file(&marker).unwrap();
    make_dirty(b"updated!\n");
    git(&["config", "core.fsmonitor", helper.to_str().unwrap()]);

    for key in ["clean", "smudge", "process"] {
        let setting = format!("filter.marker.{key}");
        let effective = hardened_git_command_for_repo(&repo)
            .unwrap()
            .arg("-C")
            .arg(&repo)
            .args(["config", "--get", &setting])
            .output()
            .unwrap();
        assert!(effective.status.success(), "{setting}");
        assert!(
            String::from_utf8(effective.stdout)
                .unwrap()
                .trim()
                .is_empty(),
            "{setting} must be overridden before any worktree read"
        );
    }
    let required = hardened_git_command_for_repo(&repo)
        .unwrap()
        .arg("-C")
        .arg(&repo)
        .args(["config", "--get", "filter.marker.required"])
        .output()
        .unwrap();
    assert!(required.status.success());
    assert_eq!(String::from_utf8(required.stdout).unwrap().trim(), "false");

    let fsmonitor = hardened_git_command_for_repo(&repo)
        .unwrap()
        .arg("-C")
        .arg(&repo)
        .args(["config", "--get", "core.fsmonitor"])
        .output()
        .unwrap();
    assert!(fsmonitor.status.success());
    assert_eq!(String::from_utf8(fsmonitor.stdout).unwrap().trim(), "false");

    assert!(
        ensure_clean_snapshot(&repo).is_err(),
        "modified file must remain visible"
    );
    assert!(
        !marker.exists(),
        "repo export must not run the configured filter"
    );
}

#[test]
fn export_preflight_refuses_head_gitlink_removed_from_index() {
    let fixture = tempfile::tempdir().unwrap();
    let repo = fixture.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    let commit = |message: &str| {
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            message,
        ]);
    };
    git(&["init", "-q"]);
    std::fs::write(repo.join("README"), "fixture\n").unwrap();
    git(&["add", "README"]);
    commit("base");

    ensure_clean_snapshot(&repo).unwrap();
    preflight_tracked_inputs(&repo)
        .expect("a repository without gitlinks must pass the input preflight");

    let head = git(&["rev-parse", "HEAD"]);
    git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{head},child"),
    ]);
    commit("record gitlink");
    git(&["rm", "--cached", "-q", "child"]);
    assert_eq!(
        git(&["diff", "--cached", "--name-only"]),
        "child",
        "index must differ from HEAD"
    );
    ensure_clean_snapshot(&repo).expect("status ignores the staged gitlink removal");

    let error = preflight_tracked_inputs(&repo).unwrap_err().to_string();
    assert!(
        error.contains("tracked special file \"child\" with Git mode 160000"),
        "{error}"
    );
}
