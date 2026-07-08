//! End-to-end integration test for `kkernel kg commit` (ADR-102 Amendment to
//! ADR-020 — the tier-2 change-set commit primitive).
//!
//! Drives the compiled `kkernel` binary rather than calling `cmd_commit` in
//! process: on an error-severity finding it calls `std::process::exit`
//! directly (mirroring `cmd_validate`'s documented exit-code contract, see
//! `kg_validate_builtin_rule_classes.rs`), which would kill the test process
//! if invoked in-tree. Spawning the binary also exercises the real CLI
//! argument parsing end-to-end.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn kkernel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kkernel")
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("git {} failed to spawn: {e}", args.join(" ")));
    assert!(
        status.success(),
        "git {} exited with {}",
        args.join(" "),
        status
    );
}

/// Initializes a git repo hermetically. `core.hooksPath` is persisted (not a
/// transient `-c` flag) so it also applies to the `git commit` the
/// `kkernel` binary itself runs under test, keeping the machine-wide
/// `check-json-data.sh` leak guard out of this test's way regardless of
/// `kg commit`'s own `KHIVE_ALLOW_DATA=1` bypass.
fn init_repo(dir: &Path) {
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    run_git(dir, &["config", "core.hooksPath", "/dev/null"]);
    run_git(dir, &["commit", "--allow-empty", "-m", "init"]);
}

fn envelope_line() -> String {
    serde_json::json!({
        "schema_version": 1,
        "producer": "pipeline:acceptance-test",
        "producer_model_family": "family:sonnet",
        "staged_at": 2_000_000_u64,
    })
    .to_string()
}

fn write_changeset(dir: &Path, name: &str, op_lines: &[String]) -> std::path::PathBuf {
    let mut content = envelope_line();
    content.push('\n');
    for line in op_lines {
        content.push_str(line);
        content.push('\n');
    }
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write change-set");
    path
}

fn clean_create_op(id: &str, name: &str) -> String {
    serde_json::json!({
        "op": "create",
        "id": id,
        "namespace": "local",
        "target": {
            "kind": "entity",
            "entity_kind": "concept",
            "name": name,
            "properties": {},
            "tags": [],
        },
    })
    .to_string()
}

fn invalid_note_kind_op(id: &str) -> String {
    serde_json::json!({
        "op": "create",
        "id": id,
        "namespace": "local",
        "target": {
            "kind": "note",
            "note_kind": "not_a_real_kind",
            "content": "hello",
            "properties": {},
            "tags": [],
        },
    })
    .to_string()
}

#[test]
fn kg_commit_lands_a_clean_changeset_with_provenance_trailers() {
    let repo = TempDir::new().expect("repo tmp");
    init_repo(repo.path());
    let stage = TempDir::new().expect("stage tmp");

    let changeset = write_changeset(
        stage.path(),
        "batch.ndjson",
        &[clean_create_op(
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "Alpha",
        )],
    );
    let rules = stage.path().join("rules.toml");
    std::fs::write(&rules, "").expect("write empty rules.toml");

    let output = Command::new(kkernel_bin())
        .args([
            "kg",
            "commit",
            changeset.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--repo",
            repo.path().to_str().unwrap(),
            "-m",
            "acceptance batch",
        ])
        .output()
        .expect("run kkernel kg commit");

    assert!(
        output.status.success(),
        "clean change-set must commit; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log_output = Command::new("git")
        .args(["log", "-1", "--pretty=%B"])
        .current_dir(repo.path())
        .output()
        .expect("git log");
    let message = String::from_utf8_lossy(&log_output.stdout);
    assert!(message.contains("acceptance batch"), "{message}");
    assert!(
        message.contains("Change-Set-Producer: pipeline:acceptance-test"),
        "{message}"
    );
    assert!(
        message.contains("Change-Set-Producer-Batch: pipeline:acceptance-test@2000000us"),
        "{message}"
    );

    assert!(
        repo.path()
            .join(".khive/kg/changesets/batch.ndjson")
            .exists(),
        "committed change-set file must be staged into the repo"
    );
}

#[test]
fn kg_commit_refuses_repo_with_configured_remote() {
    let repo = TempDir::new().expect("repo tmp");
    init_repo(repo.path());
    run_git(
        repo.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/repo.git",
        ],
    );
    let stage = TempDir::new().expect("stage tmp");

    let changeset = write_changeset(
        stage.path(),
        "batch.ndjson",
        &[clean_create_op(
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            "Beta",
        )],
    );
    let rules = stage.path().join("rules.toml");
    std::fs::write(&rules, "").expect("write empty rules.toml");

    let output = Command::new(kkernel_bin())
        .args([
            "kg",
            "commit",
            changeset.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--repo",
            repo.path().to_str().unwrap(),
            "-m",
            "should not land",
        ])
        .output()
        .expect("run kkernel kg commit");

    assert!(
        !output.status.success(),
        "a repo with a configured remote must refuse (ADR-102 D6)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("local-only"), "{stderr}");

    let log_output = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(repo.path())
        .output()
        .expect("git log");
    let log = String::from_utf8_lossy(&log_output.stdout);
    assert_eq!(
        log.lines().count(),
        1,
        "only the init commit may exist after a refused commit: {log}"
    );
}

#[test]
fn kg_commit_refuses_changeset_with_error_severity_finding() {
    let repo = TempDir::new().expect("repo tmp");
    init_repo(repo.path());
    let stage = TempDir::new().expect("stage tmp");

    // `note_kind` is a free-form string in the change-set model — an
    // unregistered kind trips `valid-note-kinds` (error severity), which
    // must refuse the commit before any git operation runs.
    let changeset = write_changeset(
        stage.path(),
        "batch.ndjson",
        &[invalid_note_kind_op("cccccccc-cccc-cccc-cccc-cccccccccccc")],
    );
    let rules = stage.path().join("rules.toml");
    std::fs::write(&rules, "").expect("write empty rules.toml");

    let output = Command::new(kkernel_bin())
        .args([
            "kg",
            "commit",
            changeset.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--repo",
            repo.path().to_str().unwrap(),
            "--format",
            "json",
            "-m",
            "should not land",
        ])
        .output()
        .expect("run kkernel kg commit");

    assert!(
        !output.status.success(),
        "an error-severity finding must refuse the commit"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be JSON: {e}\n{stdout}"));
    assert_eq!(report["summary"]["passed"], false);
    assert!(report["summary"]["errors"].as_u64().unwrap() >= 1);

    let log_output = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(repo.path())
        .output()
        .expect("git log");
    let log = String::from_utf8_lossy(&log_output.stdout);
    assert_eq!(
        log.lines().count(),
        1,
        "only the init commit may exist after a refused commit: {log}"
    );
}

#[test]
fn kg_commit_fails_loud_on_malformed_changeset() {
    let repo = TempDir::new().expect("repo tmp");
    init_repo(repo.path());
    let stage = TempDir::new().expect("stage tmp");

    let changeset = stage.path().join("garbage.ndjson");
    std::fs::write(&changeset, "not valid ndjson-delta\n").expect("write garbage");
    let rules = stage.path().join("rules.toml");
    std::fs::write(&rules, "").expect("write empty rules.toml");

    let output = Command::new(kkernel_bin())
        .args([
            "kg",
            "commit",
            changeset.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--repo",
            repo.path().to_str().unwrap(),
            "-m",
            "should not land",
        ])
        .output()
        .expect("run kkernel kg commit");

    assert!(
        !output.status.success(),
        "malformed change-set must fail loud"
    );
}
