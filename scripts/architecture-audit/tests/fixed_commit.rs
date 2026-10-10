//! Owner-native controls exercise the actual binary against immutable Git objects.
//! These tests are authored here; no fixture or binary was executed by the specialist.
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};
fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet"]);
    git(
        dir.path(),
        &["config", "user.name", "Architecture audit fixture"],
    );
    git(
        dir.path(),
        &["config", "user.email", "audit-fixture@example.invalid"],
    );
    std::fs::create_dir_all(dir.path().join("crates/demo/src")).unwrap();
    std::fs::write(
        dir.path().join("crates/demo/Cargo.toml"),
        "[package]\nname='demo'\nversion='0.1.0'\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("crates/demo/src/lib.rs"),
        "const Q: &str = \"SELECT id FROM first\";\n",
    )
    .unwrap();
    dir
}
fn commit(repo: &Path) -> String {
    git(repo, &["add", "."]);
    git(repo, &["commit", "--quiet", "-m", "fixture"]);
    git(repo, &["rev-parse", "HEAD"])
}
fn audit(repo: &Path, oid: &str) -> (Output, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_architecture-audit"))
        .args([
            "--repo",
            repo.to_str().unwrap(),
            "--commit",
            oid,
            "--crate",
            "demo",
            "--json",
        ])
        .output()
        .unwrap();
    let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "JSON {e}: {} / {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, report)
}
fn refusal(output: &Output, report: &Value) {
    assert!(!output.status.success(), "{report}");
    assert_eq!(report["complete"], false);
    let global = report["diagnostics"]
        .as_array()
        .is_some_and(|a| !a.is_empty());
    let local = report["crates"].as_array().is_some_and(|a| {
        a.iter()
            .any(|r| r["diagnostics"].as_array().is_some_and(|d| !d.is_empty()))
    });
    assert!(
        global || local,
        "refusal needs a nonempty diagnostic: {report}"
    );
}
#[test]
fn exact_two_commits_ignore_conflicting_working_tree_bytes() {
    let dir = fixture();
    let first = commit(dir.path());
    std::fs::write(dir.path().join("crates/demo/src/lib.rs"), "const Q: &str = \"SELECT id FROM first\"; const R: &str = \"UPDATE second SET ready=1\";\n").unwrap();
    let second = commit(dir.path());
    assert_ne!(first, second);
    std::fs::write(
        dir.path().join("crates/demo/src/lib.rs"),
        "fn working_tree_is_broken(",
    )
    .unwrap();
    for (oid, count) in [(&first, 1), (&second, 2)] {
        let (output, report) = audit(dir.path(), oid);
        assert!(output.status.success(), "{report}");
        assert_eq!(report["audited_commit"], oid.as_str());
        assert_eq!(report["complete"], true);
        assert_eq!(report["crates"][0]["production_sql"], count);
        assert!(!report["crates"][0]["inline_sites"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(report["input_blobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |b| b["path"] == "crates/demo/src/lib.rs" && b["oid"].as_str().unwrap().len() == 40
            ));
    }
    let (output, report) = audit(dir.path(), "HEAD");
    refusal(&output, &report);
    let (output, report) = audit(dir.path(), &"f".repeat(40));
    refusal(&output, &report);
}
#[test]
fn missing_root_malformed_reachable_and_decode_failures_are_nonzero() {
    for defect in ["missing", "malformed", "decode"] {
        let dir = fixture();
        let baseline = commit(dir.path());
        let (output, report) = audit(dir.path(), &baseline);
        assert!(output.status.success(), "{report}");
        assert_eq!(report["crates"][0]["production_sql"], 1);
        match defect {
            "missing" => {
                std::fs::write(
                    dir.path().join("crates/demo/Cargo.toml"),
                    "[package]\nname='demo'\nversion='0.1.0'\n[lib]\npath='missing.rs'\n",
                )
                .unwrap();
            }
            "malformed" => {
                std::fs::write(
                    dir.path().join("crates/demo/src/lib.rs"),
                    "mod broken; const Q: &str = \"SELECT id FROM real\";",
                )
                .unwrap();
                std::fs::write(
                    dir.path().join("crates/demo/src/broken.rs"),
                    "fn incomplete(",
                )
                .unwrap();
            }
            "decode" => {
                std::fs::write(dir.path().join("crates/demo/src/lib.rs"), [0xff, 0xfe]).unwrap();
            }
            _ => unreachable!(),
        }
        let selected = commit(dir.path());
        let (output, report) = audit(dir.path(), &selected);
        refusal(&output, &report);
        let (output, report) = audit(dir.path(), &baseline);
        assert!(
            output.status.success(),
            "old commit remains readable: {report}"
        );
    }
}
#[cfg(unix)]
#[test]
fn symlink_source_never_reads_a_host_target() {
    let dir = fixture();
    let baseline = commit(dir.path());
    std::fs::remove_file(dir.path().join("crates/demo/src/lib.rs")).unwrap();
    std::os::unix::fs::symlink(
        "/definitely-not-a-source-file",
        dir.path().join("crates/demo/src/lib.rs"),
    )
    .unwrap();
    let selected = commit(dir.path());
    let (output, report) = audit(dir.path(), &selected);
    refusal(&output, &report);
    assert!(report["diagnostics"]
        .to_string()
        .contains("regular Git blob"));
    let (output, report) = audit(dir.path(), &baseline);
    assert!(output.status.success(), "{report}");
}
