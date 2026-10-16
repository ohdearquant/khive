//! Black-box coverage for the ADR-147 offline repository-showcase CLI.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn kkernel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kkernel")
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-08-07T15:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-08-07T15:00:00Z")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .output()
        .expect("git available");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output utf-8")
        .trim()
        .to_string()
}

fn fixture_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).expect("create fixture source");
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"showcase-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write manifest");
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub mod greet;\npub use greet::greeting;\n",
    )
    .expect("write lib");
    std::fs::write(
        repo.join("src/greet.rs"),
        "pub fn greeting() -> &'static str { \"hello\" }\n",
    )
    .expect("write module");
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.com"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial structure"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/showcase-fixture.git",
        ],
    );
    repo
}

fn isolated_command(home: &Path) -> Command {
    let mut command = Command::new(kkernel_bin());
    command
        .env("HOME", home)
        .env("KHIVE_VOLUME_LOCK_DIR", home.join("volume-locks"))
        .env("KHIVE_NO_DAEMON", "1")
        .env("GH_CONFIG_DIR", home.join("gh"))
        .env_remove("KHIVE_CONFIG")
        .env_remove("KHIVE_DB")
        .env_remove("KHIVE_PACKS")
        .env_remove("KHIVE_ACTOR")
        .env_remove("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    command
}

fn build(home: &Path, repo: &Path, work: &Path, output: &Path, include: &str) -> Output {
    let revision = git(repo, &["rev-parse", "HEAD"]);
    isolated_command(home)
        .args([
            "repo",
            "build",
            "--source",
            repo.to_str().expect("utf-8 repo path"),
            "--revision",
            &revision,
            "--work-dir",
            work.to_str().expect("utf-8 work path"),
            "--include",
            include,
            "--tags",
            "none",
            "--default-branch",
            "main",
            "--generated-at",
            "2026-08-07T12:00:00-04:00",
            "--out",
            output.to_str().expect("utf-8 output path"),
        ])
        .output()
        .expect("run repo build")
}

#[cfg(unix)]
fn init_filter_fixture_repo(repo: &Path, filtered: bool) {
    std::fs::create_dir_all(repo).expect("create fixture repository");
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.name", "Fixture"]);
    git(repo, &["config", "user.email", "fixture@example.invalid"]);
    if filtered {
        std::fs::write(repo.join(".gitattributes"), "tracked.txt filter=probe\n")
            .expect("write filter attributes");
        std::fs::write(repo.join("tracked.txt"), b"original\n").expect("write tracked input");
        git(repo, &["add", ".gitattributes", "tracked.txt"]);
    } else {
        std::fs::write(repo.join("README"), b"fixture\n").expect("write root input");
        git(repo, &["add", "README"]);
    }
    git(repo, &["commit", "-m", "initial fixture"]);
}

#[cfg(unix)]
fn assert_filter_is_not_run_before_refusal(root: &Path, submodule: bool) {
    use std::os::unix::fs::PermissionsExt;

    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("create isolated home");
    let repo = root.join("repo");
    init_filter_fixture_repo(&repo, !submodule);
    let target = if submodule {
        let child = repo.join("child");
        init_filter_fixture_repo(&child, true);
        std::fs::write(
            repo.join(".gitmodules"),
            "[submodule \"child\"]\n\tpath = child\n\turl = ./child\n",
        )
        .expect("write local submodule record");
        git(&repo, &["add", ".gitmodules", "child"]);
        git(&repo, &["commit", "-m", "record child gitlink"]);
        git(&repo, &["config", "submodule.child.ignore", "none"]);
        assert!(
            git(&repo, &["ls-files", "--stage", "child"]).starts_with("160000"),
            "fixture must record a gitlink"
        );
        child
    } else {
        repo.clone()
    };

    let helper = root.join("marker-filter.sh");
    let marker = root.join("marker-filter.sh.marker");
    std::fs::write(&helper, "#!/bin/sh\n: > \"$0.marker\"\ncat\n").expect("write marker filter");
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))
        .expect("make marker filter executable");
    let helper_command = format!("'{}'", helper.display().to_string().replace('\'', "'\\''"));
    git(&target, &["config", "filter.probe.clean", &helper_command]);
    let tracked = target.join("tracked.txt");
    let write_dirty_input = |bytes: &[u8]| {
        assert_eq!(bytes.len(), b"original\n".len());
        std::fs::write(&tracked, bytes).expect("modify filtered input");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&tracked)
            .expect("open filtered input")
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(946_684_800))
            .expect("force stale filtered input mtime");
    };
    write_dirty_input(b"changed!\n");
    let control = git(
        &repo,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    );
    assert!(!control.is_empty(), "fixture must be dirty");
    if marker.exists() {
        std::fs::remove_file(&marker).expect("clear status marker");
    }
    git(
        &target,
        &[
            "hash-object",
            "--filters",
            "--path=tracked.txt",
            "--",
            "tracked.txt",
        ],
    );
    assert!(
        marker.is_file(),
        "explicit Git probe must run the marker filter"
    );
    std::fs::remove_file(&marker).expect("clear positive-control marker");
    write_dirty_input(b"updated!\n");

    let output = root.join("bundle.json");
    let history = root.join("history.db");
    let map = root.join("map.db");
    let mut command = isolated_command(&home);
    command
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE");
    if submodule {
        command.env_remove("GIT_CONFIG");
    } else {
        command.env("GIT_CONFIG", "/dev/null");
    }
    let result = command
        .args([
            "repo",
            "export",
            "--repo",
            repo.to_str().expect("utf-8 repo path"),
            "--history-db",
            history.to_str().expect("utf-8 history path"),
            "--map-db",
            map.to_str().expect("utf-8 map path"),
            "--repository-url",
            "https://github.com/example/synthetic",
            "--generated-at",
            "2026-08-07T16:00:00Z",
            "--out",
            output.to_str().expect("utf-8 output path"),
        ])
        .output()
        .expect("run guarded repo export");
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !result.status.success(),
        "export must refuse: {diagnostics}"
    );
    assert!(
        diagnostics.contains("not a clean HEAD snapshot")
            || diagnostics.contains("tracked special file"),
        "export must reach the snapshot or gitlink refusal: {diagnostics}"
    );
    assert!(
        !marker.exists(),
        "Git clean filter ran before export refusal: {diagnostics}"
    );
    assert!(!output.exists() && !history.exists() && !map.exists());
}

#[cfg(unix)]
#[test]
fn inherited_git_config_does_not_run_clean_filter() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    assert_filter_is_not_run_before_refusal(tmp.path(), false);
}

#[cfg(unix)]
#[test]
fn submodule_local_filter_is_not_run_before_gitlink_refusal() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    assert_filter_is_not_run_before_refusal(tmp.path(), true);
}

include!("repo_showcase_cli_tests/canonical_output_and_export_tests.rs");

include!("repo_showcase_cli_tests/source_and_argument_validation_tests.rs");

#[cfg(unix)]
#[test]
fn tracked_symlink_is_rejected_before_ingest() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let repo = fixture_repo(tmp.path());
    let outside = tmp.path().join("outside.rs");
    std::fs::write(&outside, "pub const OUTSIDE: bool = true;\n").expect("write outside file");
    symlink(&outside, repo.join("src/outside.rs")).expect("create tracked outside symlink");
    git(&repo, &["add", "src/outside.rs"]);
    git(&repo, &["commit", "-m", "add outside symlink"]);

    let work = tmp.path().join("work");
    let bundle = tmp.path().join("bundle.json");
    let output = build(&home, &repo, &work, &bundle, "commits");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("tracked symlink"), "{stderr}");
    assert!(!work.join("history.db").exists());
    assert!(!work.join("code-map.db").exists());
    assert!(!bundle.exists());
}

#[cfg(unix)]
#[test]
fn export_preflight_rejects_symlink_and_oversized_manifest() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let repo = fixture_repo(tmp.path());
    let work = tmp.path().join("work");
    let baseline = tmp.path().join("baseline.json");
    let baseline_run = build(&home, &repo, &work, &baseline, "commits");
    assert!(
        baseline_run.status.success(),
        "baseline build failed: {}",
        String::from_utf8_lossy(&baseline_run.stderr)
    );
    let history_db = work.join("history.db");
    let map_db = work.join("code-map.db");

    let run_export = |output: &Path| {
        isolated_command(&home)
            .args([
                "repo",
                "export",
                "--repo",
                repo.to_str().unwrap(),
                "--history-db",
                history_db.to_str().unwrap(),
                "--map-db",
                map_db.to_str().unwrap(),
                "--repository-url",
                "https://github.com/example/showcase-fixture",
                "--generated-at",
                "2026-08-07T16:00:00Z",
                "--out",
                output.to_str().unwrap(),
            ])
            .output()
            .expect("run guarded repo export")
    };

    let outside = tmp.path().join("outside.rs");
    std::fs::write(&outside, "pub const OUTSIDE: bool = true;\n").expect("write outside file");
    symlink(&outside, repo.join("src/outside.rs")).expect("create tracked outside symlink");
    git(&repo, &["add", "src/outside.rs"]);
    git(&repo, &["commit", "-m", "add outside symlink"]);
    let symlink_bundle = tmp.path().join("symlink-export.json");
    let symlink_output = run_export(&symlink_bundle);
    assert!(!symlink_output.status.success());
    assert!(String::from_utf8_lossy(&symlink_output.stderr).contains("tracked symlink"));
    assert!(!symlink_bundle.exists());

    git(&repo, &["rm", "src/outside.rs"]);
    std::fs::write(repo.join("Cargo.toml"), vec![b'#'; 1024 * 1024 + 1])
        .expect("write oversized Cargo manifest");
    git(&repo, &["add", "Cargo.toml"]);
    git(
        &repo,
        &["commit", "-m", "replace symlink with oversized manifest"],
    );
    let manifest_bundle = tmp.path().join("manifest-export.json");
    let manifest_output = run_export(&manifest_bundle);
    assert!(!manifest_output.status.success());
    assert!(String::from_utf8_lossy(&manifest_output.stderr).contains("tracked Cargo.toml"));
    assert!(!manifest_bundle.exists());
}

#[test]
fn oversized_rust_source_and_manifest_are_rejected_before_ingest() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");

    let rust_root = tmp.path().join("rust-case");
    let rust_repo = fixture_repo(&rust_root);
    std::fs::write(
        rust_repo.join("src/oversized.rs"),
        vec![b' '; 8 * 1024 * 1024 + 1],
    )
    .expect("write oversized Rust source");
    git(&rust_repo, &["add", "src/oversized.rs"]);
    git(&rust_repo, &["commit", "-m", "add oversized Rust source"]);
    let rust_work = tmp.path().join("rust-work");
    let rust_bundle = tmp.path().join("rust.json");
    let rust_output = build(&home, &rust_repo, &rust_work, &rust_bundle, "commits");
    assert!(!rust_output.status.success());
    let rust_stderr = String::from_utf8_lossy(&rust_output.stderr);
    assert!(rust_stderr.contains("tracked Rust source"), "{rust_stderr}");
    assert!(rust_stderr.contains("8388608-byte"), "{rust_stderr}");
    assert!(!rust_work.join("history.db").exists());

    let manifest_root = tmp.path().join("manifest-case");
    let manifest_repo = fixture_repo(&manifest_root);
    std::fs::write(
        manifest_repo.join("Cargo.toml"),
        vec![b'#'; 1024 * 1024 + 1],
    )
    .expect("write oversized Cargo manifest");
    git(&manifest_repo, &["add", "Cargo.toml"]);
    git(
        &manifest_repo,
        &["commit", "-m", "add oversized Cargo manifest"],
    );
    let manifest_work = tmp.path().join("manifest-work");
    let manifest_bundle = tmp.path().join("manifest.json");
    let manifest_output = build(
        &home,
        &manifest_repo,
        &manifest_work,
        &manifest_bundle,
        "commits",
    );
    assert!(!manifest_output.status.success());
    let manifest_stderr = String::from_utf8_lossy(&manifest_output.stderr);
    assert!(
        manifest_stderr.contains("tracked Cargo.toml"),
        "{manifest_stderr}"
    );
    assert!(
        manifest_stderr.contains("1048576-byte"),
        "{manifest_stderr}"
    );
    assert!(!manifest_work.join("history.db").exists());
}

#[cfg(unix)]
#[test]
fn unavailable_forge_sources_are_preserved_as_skipped() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let repo = fixture_repo(tmp.path());
    let fake_bin = tmp.path().join("bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake bin");
    let fake_gh = fake_bin.join("gh");
    std::fs::write(&fake_gh, "#!/bin/sh\nexit 1\n").expect("write fake gh");
    let mut permissions = std::fs::metadata(&fake_gh)
        .expect("fake gh metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, permissions).expect("chmod fake gh");
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let work = tmp.path().join("work");
    let bundle = tmp.path().join("bundle.json");

    let output = isolated_command(&home)
        .env("PATH", path)
        .args([
            "repo",
            "build",
            "--source",
            repo.to_str().unwrap(),
            "--work-dir",
            work.to_str().unwrap(),
            "--generated-at",
            "2026-08-07T16:00:00Z",
            "--out",
            bundle.to_str().unwrap(),
        ])
        .output()
        .expect("run repo build");
    assert!(
        output.status.success(),
        "build failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("report JSON");
    assert_eq!(report["digest"]["sources"]["issues"]["state"], "skipped");
    assert_eq!(
        report["digest"]["sources"]["pull_requests"]["state"],
        "skipped"
    );
    assert_eq!(report["digest"]["sources"]["commits"]["state"], "completed");
}
