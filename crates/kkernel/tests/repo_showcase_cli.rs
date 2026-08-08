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
        .env("KHIVE_NO_DAEMON", "1")
        .env("GH_CONFIG_DIR", home.join("gh"))
        .env_remove("KHIVE_CONFIG")
        .env_remove("KHIVE_DB")
        .env_remove("KHIVE_PACKS")
        .env_remove("KHIVE_ACTOR")
        .env_remove("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    command
}

fn build_with_symbol_tier(
    home: &Path,
    repo: &Path,
    work: &Path,
    output: &Path,
    include: &str,
    enable_l2: bool,
) -> Output {
    let revision = git(repo, &["rev-parse", "HEAD"]);
    let mut command = isolated_command(home);
    command.args(["repo", "build"]);
    if enable_l2 {
        command.arg("--enable-l2");
    }
    command
        .args([
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

fn build(home: &Path, repo: &Path, work: &Path, output: &Path, include: &str) -> Output {
    build_with_symbol_tier(home, repo, work, output, include, false)
}

#[test]
fn l2_enabled_build_reports_and_exports_symbol_provenance() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let repo = fixture_repo(tmp.path());
    let output_path = tmp.path().join("bundle.json");

    let output = build_with_symbol_tier(
        &home,
        &repo,
        &tmp.path().join("work"),
        &output_path,
        "commits",
        true,
    );
    assert!(
        output.status.success(),
        "L2 build failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let head = git(&repo, &["rev-parse", "HEAD"]);
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("build report JSON");
    assert_eq!(report["code"]["source_revision"], head);
    for counter in [
        "symbols_created",
        "symbols_updated",
        "symbol_dependencies_unresolved",
        "symbol_edges_stamped",
        "symbol_parse_failures",
    ] {
        assert!(
            report["code"][counter].is_u64(),
            "L2 report omitted {counter}"
        );
    }
    assert!(report["code"]["symbols_created"].as_u64().unwrap() > 0);

    let bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output_path).expect("L2 bundle bytes"))
            .expect("L2 bundle JSON");
    let provenance = &bundle["meta"]["ingest"]["code_ingest"]["value"]["l2"];
    assert_eq!(provenance["source_revision"], head);
    for counter in [
        "symbols_created",
        "symbols_updated",
        "symbol_dependencies_unresolved",
        "symbol_edges_stamped",
        "symbol_parse_failures",
    ] {
        assert_eq!(provenance[counter], report["code"][counter]);
    }
    for page in ["functions", "datatypes", "interfaces"] {
        assert_eq!(bundle["graph"][page]["total_count"]["status"], "available");
        assert_eq!(bundle["graph"][page]["disclosure"]["status"], "complete");
        assert_eq!(
            bundle["graph"][page]["bound"]["order"],
            "module_path,name,symbol_id"
        );
    }
}

#[test]
fn independent_commits_only_builds_are_byte_identical() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let repo = fixture_repo(tmp.path());
    let first = tmp.path().join("first.json");
    let second = tmp.path().join("second.json");

    let first_run = build(
        &home,
        &repo,
        &tmp.path().join("work-first"),
        &first,
        "commits",
    );
    assert!(
        first_run.status.success(),
        "first build failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&first_run.stdout),
        String::from_utf8_lossy(&first_run.stderr)
    );
    let first_report: serde_json::Value =
        serde_json::from_slice(&first_run.stdout).expect("build report JSON");
    assert_eq!(first_report["head_sha"], git(&repo, &["rev-parse", "HEAD"]));
    assert_eq!(
        first_report["digest"]["sources"]["commits"]["state"],
        "completed"
    );
    assert!(first_report["digest"]["sources"]["issues"].is_null());

    let second_run = build(
        &home,
        &repo,
        &tmp.path().join("work-second"),
        &second,
        "commits",
    );
    assert!(
        second_run.status.success(),
        "second build failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&second_run.stdout),
        String::from_utf8_lossy(&second_run.stderr)
    );
    assert_eq!(
        std::fs::read(&first).expect("first bundle"),
        std::fs::read(&second).expect("second bundle"),
        "random storage UUIDs or ingest clocks leaked into canonical bundle bytes"
    );
    let bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(first).expect("bundle bytes")).expect("bundle JSON");
    assert_eq!(bundle["schema_version"], "khive.repo.v1");
    assert_eq!(
        bundle["meta"]["ingest"]["git_digest"]["status"],
        "available"
    );
    assert_eq!(
        bundle["meta"]["ingest"]["git_digest"]["value"]["sources"]["issues"]["state"],
        "unrequested"
    );
    assert_eq!(
        bundle["meta"]["ingest"]["code_ingest"]["value"]["languages"],
        serde_json::json!(["rust"])
    );
    assert_eq!(
        bundle["meta"]["ingest"]["clone_tags"]["state"],
        "unrequested"
    );
    assert_eq!(
        bundle["meta"]["repository"]["default_branch"]["value"],
        "main"
    );
    let resolution = &bundle["graph"]["join_resolution"]["repositories"]["value"][0];
    assert_eq!(resolution["files"], 2);
    assert_eq!(resolution["derived_keys"], 2);
    assert_eq!(resolution["entity_keys"], 2);
    assert_eq!(resolution["matched"], 2);
    assert_eq!(resolution["resolution_rate"]["value"], 1.0);
    assert_eq!(resolution["residuals"]["items"], serde_json::json!([]));
    let historical = &bundle["graph"]["join_resolution"]["historical"]["value"][0];
    assert_eq!(historical["total_changed_paths"], 3);
    assert_eq!(historical["rust_in_scope_paths"], 2);
    assert_eq!(historical["matched_rust_paths"], 2);
    assert_eq!(historical["out_of_scope_paths"], 1);
    assert_eq!(
        historical["unresolved_rust_paths"]["items"],
        serde_json::json!([])
    );
}

#[test]
fn export_reads_existing_stores_without_running_ingest() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let repo = fixture_repo(tmp.path());
    let work = tmp.path().join("work");
    let built = tmp.path().join("built.json");
    let build_run = build(&home, &repo, &work, &built, "commits");
    assert!(
        build_run.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build_run.stderr)
    );

    let exported = tmp.path().join("exported.json");
    let history_db = work.join("history.db");
    let map_db = work.join("code-map.db");
    let output = isolated_command(&home)
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
            exported.to_str().unwrap(),
        ])
        .output()
        .expect("run repo export");
    assert!(
        output.status.success(),
        "export failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(exported.is_file());
    let bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(exported).expect("exported bundle"))
            .expect("exported bundle JSON");
    assert_eq!(
        bundle["meta"]["ingest"]["git_digest"]["status"],
        "unavailable"
    );
    assert_eq!(
        bundle["meta"]["repository"]["default_branch"]["status"],
        "unavailable"
    );

    let history_before = std::fs::read(&history_db).expect("history database before collision");
    let collision = isolated_command(&home)
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
            history_db.to_str().unwrap(),
        ])
        .output()
        .expect("run colliding repo export");
    assert!(!collision.status.success());
    assert!(String::from_utf8_lossy(&collision.stderr).contains("must not overwrite"));
    assert_eq!(
        std::fs::read(&history_db).expect("history database after collision"),
        history_before
    );
}

#[test]
fn build_refuses_store_reuse_and_missing_commits() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let repo = fixture_repo(tmp.path());
    let work = tmp.path().join("work");
    let output = tmp.path().join("bundle.json");
    let first = build(&home, &repo, &work, &output, "commits");
    assert!(first.status.success());

    let reuse = build(&home, &repo, &work, &output, "commits");
    assert!(!reuse.status.success());
    assert!(String::from_utf8_lossy(&reuse.stderr).contains("already exists"));

    let no_commits = build(
        &home,
        &repo,
        &tmp.path().join("no-commits"),
        &tmp.path().join("no-commits.json"),
        "issues",
    );
    assert!(!no_commits.status.success());
    assert!(String::from_utf8_lossy(&no_commits.stderr).contains("requires commits"));

    let inside = build(
        &home,
        &repo,
        &repo.join(".showcase-work"),
        &tmp.path().join("inside.json"),
        "commits",
    );
    assert!(!inside.status.success());
    assert!(String::from_utf8_lossy(&inside.stderr).contains("must be outside"));
    assert!(!repo.join(".showcase-work").exists());
}

#[test]
fn local_revision_must_be_head_and_generated_at_must_follow_it() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let repo = fixture_repo(tmp.path());
    let first_revision = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(
        repo.join("src/greet.rs"),
        "pub fn greeting() -> &'static str { \"hello, v2\" }\n",
    )
    .expect("update fixture module");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "update greeting"]);

    let revision_work = tmp.path().join("revision-work");
    let revision_bundle = tmp.path().join("revision.json");
    let revision_run = isolated_command(&home)
        .args([
            "repo",
            "build",
            "--source",
            repo.to_str().unwrap(),
            "--revision",
            &first_revision,
            "--work-dir",
            revision_work.to_str().unwrap(),
            "--include",
            "commits",
            "--generated-at",
            "2026-08-07T16:00:00Z",
            "--out",
            revision_bundle.to_str().unwrap(),
        ])
        .output()
        .expect("run pinned local build");
    assert!(!revision_run.status.success());
    assert!(String::from_utf8_lossy(&revision_run.stderr).contains("local source HEAD"));

    let timestamp_work = tmp.path().join("timestamp-work");
    let timestamp_bundle = tmp.path().join("timestamp.json");
    let timestamp_run = isolated_command(&home)
        .args([
            "repo",
            "build",
            "--source",
            repo.to_str().unwrap(),
            "--work-dir",
            timestamp_work.to_str().unwrap(),
            "--include",
            "commits",
            "--generated-at",
            "2026-08-07T14:59:59Z",
            "--out",
            timestamp_bundle.to_str().unwrap(),
        ])
        .output()
        .expect("run build with pre-snapshot timestamp");
    assert!(!timestamp_run.status.success());
    assert!(String::from_utf8_lossy(&timestamp_run.stderr).contains("predates HEAD commit time"));
}

#[test]
fn remote_urls_reject_sensitive_components_before_clone_resolution() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let cases = [
        (
            "https://agent:sensitive-password@example.invalid/owner/repository",
            "userinfo or credentials",
        ),
        (
            "https://example.invalid/owner/repository?token=sensitive-query",
            "query component",
        ),
        (
            "https://example.invalid/owner/repository#sensitive-fragment",
            "fragment component",
        ),
    ];

    for (index, (source, expected)) in cases.into_iter().enumerate() {
        let work = tmp.path().join(format!("work-{index}"));
        let bundle = tmp.path().join(format!("bundle-{index}.json"));
        let output = isolated_command(&home)
            .args([
                "repo",
                "build",
                "--source",
                source,
                "--work-dir",
                work.to_str().unwrap(),
                "--include",
                "commits",
                "--generated-at",
                "2026-08-07T16:00:00Z",
                "--out",
                bundle.to_str().unwrap(),
            ])
            .output()
            .expect("run build with unsafe remote URL");
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected),
            "expected {expected:?} rejection, got {stderr}"
        );
        assert!(!stderr.contains("sensitive-"), "secret leaked: {stderr}");
        assert!(
            !work.exists(),
            "unsafe URL reached worktree/cache materialization"
        );
        assert!(!bundle.exists());
    }
}

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
