//! Binary-level, private-repository acceptance for the ADR-020 Git diff renderer.

use std::path::Path;
use std::process::{Command, Output};

use khive_repo_showcase::git_safety::hardened_git_command;
use serde_json::{json, Value};
use tempfile::TempDir;

const A: &str = "10000000-0000-4000-8000-000000000001";
const B: &str = "10000000-0000-4000-8000-000000000002";
const C: &str = "10000000-0000-4000-8000-000000000003";
const D: &str = "10000000-0000-4000-8000-000000000004";
const E1: &str = "20000000-0000-4000-8000-000000000001";
const E2: &str = "20000000-0000-4000-8000-000000000002";
const E3: &str = "20000000-0000-4000-8000-000000000003";

fn git(repo: &Path, args: &[&str]) -> Vec<u8> {
    let output = hardened_git_command()
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("repo/.khive/kg")).unwrap();
    std::fs::create_dir(dir.path().join("home")).unwrap();
    let repo = dir.path().join("repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    dir
}

fn entity(id: &str, name: &str, properties: Value) -> Value {
    json!({"id": id, "kind": "concept", "name": name, "properties": properties})
}

fn edge(id: &str, source: &str, target: &str, weight: f64) -> Value {
    json!({"edge_id": id, "source": source, "target": target, "relation": "contains", "weight": weight, "properties": {}})
}

fn write(repo: &Path, file: &str, records: &[Value]) {
    let text: String = records.iter().map(|record| format!("{record}\n")).collect();
    std::fs::write(repo.join(".khive/kg").join(file), text).unwrap();
}

fn commit(repo: &Path) {
    git(repo, &["add", "--", ".khive/kg"]);
    git(repo, &["commit", "-qm", "KG fixture"]);
}

fn command(dir: &TempDir, reference: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    command
        .current_dir(dir.path().join("repo"))
        .env("HOME", dir.path().join("home"))
        .env("USERPROFILE", dir.path().join("home"))
        .env("KHIVE_DB", dir.path().join("must-not-open.db"))
        .args(["kg", "diff", "--repo"])
        .arg(dir.path().join("repo"));
    if let Some(reference) = reference {
        command.arg("--").arg(reference);
    }
    command
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn renders_combined_staged_and_unstaged_changes_without_writes() {
    let dir = fixture();
    let repo = dir.path().join("repo");
    write(
        &repo,
        "entities.ndjson",
        &[
            entity(
                A,
                "Alpha",
                json!({"status": "old", "removed": 1, "null_before": null}),
            ),
            entity(B, "Beta", json!({})),
            entity(C, "Deleted", json!({})),
        ],
    );
    write(
        &repo,
        "edges.ndjson",
        &[edge(E1, A, B, 0.5), edge(E3, B, C, 0.5)],
    );
    commit(&repo);
    write(
        &repo,
        "entities.ndjson",
        &[
            entity(A, "Alpha", json!({"status": "staged"})),
            entity(B, "Beta", json!({})),
            entity(D, "Added", json!({})),
        ],
    );
    git(&repo, &["add", "--", ".khive/kg/entities.ndjson"]);
    write(
        &repo,
        "entities.ndjson",
        &[
            entity(D, "Added", json!({})),
            entity(B, "Beta", json!({})),
            entity(A, "Alpha", json!({"status": "final", "added": null})),
        ],
    );
    write(
        &repo,
        "edges.ndjson",
        &[edge(E2, A, D, 0.7), edge(E1, A, B, 0.9)],
    );
    let index = std::fs::read(repo.join(".git/index")).unwrap();
    let entities = std::fs::read(repo.join(".khive/kg/entities.ndjson")).unwrap();
    let edges = std::fs::read(repo.join(".khive/kg/edges.ndjson")).unwrap();
    let output = success(command(&dir, None).output().unwrap());
    for expected in [
        format!("~ entity {A} (\"concept\" \"Alpha\")"),
        format!("- entity {C} (\"concept\" \"Deleted\")"),
        format!("+ entity {D} (\"concept\" \"Added\")"),
        format!("~ edge {E1}"),
        format!("+ edge {E2}"),
        format!("- edge {E3}"),
        "properties.status: \"old\" -> \"final\"".into(),
        "properties.removed: 1 -> <absent>".into(),
        "properties.null_before: null -> <absent>".into(),
        "properties.added: <absent> -> null".into(),
        format!("{D} (\"Added\")"),
        format!("{C} (\"Deleted\")"),
    ] {
        assert!(output.contains(&expected), "missing {expected}: {output}");
    }
    assert_eq!(
        output
            .lines()
            .filter(|line| line.starts_with(['+', '-', '~']))
            .count(),
        6
    );
    assert!(!output.contains("staged"));
    assert!(output.find(A).unwrap() < output.find(C).unwrap());
    assert!(output.find(C).unwrap() < output.find(D).unwrap());
    assert_eq!(output, success(command(&dir, None).output().unwrap()));
    assert_eq!(index, std::fs::read(repo.join(".git/index")).unwrap());
    assert_eq!(
        entities,
        std::fs::read(repo.join(".khive/kg/entities.ndjson")).unwrap()
    );
    assert_eq!(
        edges,
        std::fs::read(repo.join(".khive/kg/edges.ndjson")).unwrap()
    );
    assert!(!dir.path().join("must-not-open.db").exists());
    assert!(!repo.join(".khive/state").exists());
}

#[test]
fn accepts_explicit_ref_and_ignores_record_order_and_json_layout() {
    let dir = fixture();
    let repo = dir.path().join("repo");
    write(
        &repo,
        "entities.ndjson",
        &[entity(A, "Before", json!({})), entity(B, "Beta", json!({}))],
    );
    write(&repo, "edges.ndjson", &[]);
    commit(&repo);
    write(
        &repo,
        "entities.ndjson",
        &[entity(A, "After", json!({})), entity(B, "Beta", json!({}))],
    );
    commit(&repo);
    assert_eq!(
        success(command(&dir, None).output().unwrap()),
        "No KG changes.\n"
    );
    let output = success(command(&dir, Some("HEAD~1")).output().unwrap());
    assert!(output.contains("name: \"Before\" -> \"After\""));
    std::fs::write(repo.join(".khive/kg/entities.ndjson"), format!(
        "{{\"name\": \"Beta\", \"properties\": {{}}, \"id\": \"{B}\", \"kind\": \"concept\"}}\n{{\"name\": \"After\", \"id\": \"{A}\", \"kind\": \"concept\", \"properties\": {{}}}}\n"
    )).unwrap();
    assert_eq!(
        success(command(&dir, None).output().unwrap()),
        "No KG changes.\n"
    );
}

#[test]
fn new_and_removed_files_follow_git_diff_tracking() {
    let dir = fixture();
    let repo = dir.path().join("repo");
    write(
        &repo,
        "entities.ndjson",
        &[entity(A, "Alpha", json!({})), entity(B, "Beta", json!({}))],
    );
    commit(&repo);
    write(&repo, "edges.ndjson", &[edge(E1, A, B, 0.5)]);
    assert_eq!(
        success(command(&dir, None).output().unwrap()),
        "No KG changes.\n"
    );
    git(&repo, &["add", "--", ".khive/kg/edges.ndjson"]);
    assert!(success(command(&dir, None).output().unwrap()).contains(&format!("+ edge {E1}")));
    commit(&repo);
    std::fs::remove_file(repo.join(".khive/kg/entities.ndjson")).unwrap();
    std::fs::remove_file(repo.join(".khive/kg/edges.ndjson")).unwrap();
    let output = success(command(&dir, None).output().unwrap());
    assert!(output.contains(&format!("- entity {A}")));
    assert!(output.contains(&format!("- edge {E1}")));
    assert!(output.contains(&format!("{B} (\"Beta\")")));
}

#[test]
fn invalid_refs_and_changed_records_fail_without_partial_output() {
    let dir = fixture();
    let repo = dir.path().join("repo");
    write(&repo, "entities.ndjson", &[entity(A, "Alpha", json!({}))]);
    write(&repo, "edges.ndjson", &[]);
    commit(&repo);
    for reference in ["missing-ref", "--output=unexpected-file"] {
        let output = command(&dir, Some(reference)).output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
    assert!(!repo.join("unexpected-file").exists());
    for invalid in [
        "{broken\n".into(),
        "[]\n".into(),
        entity("bad-id", "Bad", json!({})).to_string(),
        entity(A, "Bad properties", json!([])).to_string(),
        format!(
            "{}\n{}\n",
            entity(A, "First", json!({})),
            entity(A, "Second", json!({}))
        ),
    ] {
        std::fs::write(repo.join(".khive/kg/entities.ndjson"), invalid).unwrap();
        let output = command(&dir, None).output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
    write(&repo, "entities.ndjson", &[entity(A, "Changed", json!({}))]);
    std::fs::write(repo.join(".khive/kg/edges.ndjson"), "not-json\n").unwrap();
    let output = command(&dir, None).output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!dir.path().join("must-not-open.db").exists());
}

#[cfg(unix)]
#[test]
fn external_diff_textconv_and_content_filters_are_not_executed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = fixture();
    let repo = dir.path().join("repo");
    write(&repo, "entities.ndjson", &[entity(A, "Before", json!({}))]);
    write(&repo, "edges.ndjson", &[]);
    commit(&repo);
    let helper = dir.path().join("callback.sh");
    std::fs::write(&helper, "#!/bin/sh\n: > \"$0.marker\"\nexit 99\n").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let helper_command = format!("'{}'", helper.display().to_string().replace('\'', "'\\''"));
    std::fs::write(
        repo.join(".gitattributes"),
        ".khive/kg/*.ndjson diff=kg filter=kg\n",
    )
    .unwrap();
    for key in [
        "diff.external",
        "diff.kg.command",
        "diff.kg.textconv",
        "filter.kg.clean",
        "filter.kg.process",
    ] {
        git(&repo, &["config", key, &helper_command]);
    }
    git(&repo, &["config", "filter.kg.required", "true"]);
    write(&repo, "entities.ndjson", &[entity(A, "After", json!({}))]);
    let output = success(
        command(&dir, None)
            .env("GIT_EXTERNAL_DIFF", &helper_command)
            .output()
            .unwrap(),
    );
    assert!(output.contains("name: \"Before\" -> \"After\""));
    assert!(!dir.path().join("callback.sh.marker").exists());
}
