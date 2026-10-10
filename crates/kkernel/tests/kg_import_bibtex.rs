use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};
use tempfile::TempDir;

fn command(tmp: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|key| key.starts_with("KHIVE_") && key != "KHIVE_TEST_HARNESS")
        {
            command.env_remove(key);
        }
    }
    command
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("KHIVE_VOLUME_LOCK_DIR", tmp.path().join("volume-locks"));
    command
}

fn import(tmp: &TempDir, source: &Path, db: &Path) -> Command {
    let mut command = command(tmp);
    command
        .args(["kg", "import"])
        .arg(source)
        .arg("--db")
        .arg(db);
    command
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_summary(output: &Output, entities: usize, edges: usize, entries: usize, skipped: usize) {
    assert_success(output);
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        summary,
        json!({
            "entities_imported": entities,
            "edges_imported": edges,
            "edges_skipped": 0,
            "embedding_truncation": {"truncated": 0, "discarded_bytes": 0},
            "adapter": {
                "format": "bibtex",
                "entries": entries,
                "skipped": skipped,
                "warnings": skipped
            }
        })
    );
}

fn export(tmp: &TempDir, db: &Path) -> Value {
    let target = tmp.path().join("export.json");
    let result = command(tmp)
        .args(["kg", "export", "--db"])
        .arg(db)
        .arg(&target)
        .output()
        .unwrap();
    assert_success(&result);
    serde_json::from_slice(&std::fs::read(target).unwrap()).unwrap()
}

fn entity<'a>(archive: &'a Value, name: &str) -> &'a Value {
    archive["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entity| entity["name"] == name)
        .unwrap_or_else(|| panic!("missing entity {name:?}: {archive}"))
}

fn assert_papers(archive: &Value, count: usize) {
    assert_eq!(archive["format"], "khive-kg");
    assert_eq!(archive["namespace"], "local");
    let entities = archive["entities"].as_array().unwrap();
    assert_eq!(
        entities.len(),
        count,
        "authors must not create person entities"
    );
    let mut ids = std::collections::HashSet::new();
    for entity in entities {
        assert_eq!(entity["kind"], "document");
        assert_eq!(entity["entity_type"], "paper");
        let id = entity["id"].as_str().unwrap();
        assert_eq!(uuid::Uuid::parse_str(id).unwrap().to_string(), id);
        assert!(ids.insert(id), "each paper must have a distinct UUID");
    }
}

#[test]
fn explicit_and_inferred_bibtex_preserve_paper_mapping_and_expand_macros() {
    for (extension, explicit) in [("data", true), ("bib", false), ("BIB", false)] {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join(format!("papers.{extension}"));
        let db = tmp.path().join("target.db");
        std::fs::write(
            &source,
            r#"@string{people = "Ada Lovelace and Grace Hopper"}
@string{prefix = "Graph "}
@article{first,
  title = PREFIX # {Methods},
  abstract = {A nested {graph} study.},
  author = people,
  year = 2026,
  journal = {Journal of Graphs},
  booktitle = {Ignored conference},
  doi = {10.1234/graph.1},
  url = {https://example.org/first},
  archivePrefix = {arXiv},
  eprint = {2601.01234}
}
@inproceedings{second,
  title = {Graph Systems},
  abstract = "Second paper.",
  author = PEOPLE,
  year = "2025",
  booktitle = {Graph Conference},
  doi = {10.1234/graph.2},
  url = {https://example.org/second},
  archivePrefix = {OtherArchive},
  eprint = {ignored-eprint}
}
"#,
        )
        .unwrap();
        let mut cmd = import(&tmp, &source, &db);
        if explicit {
            cmd.args(["--format", "bibtex"]);
        }
        assert_summary(&cmd.output().unwrap(), 2, 0, 2, 0);
        let archive = export(&tmp, &db);
        assert_papers(&archive, 2);
        let first = entity(&archive, "Graph Methods");
        assert_eq!(first["description"], "A nested {graph} study.");
        assert_eq!(
            first["properties"],
            json!({
                "authors": "Ada Lovelace and Grace Hopper",
                "year": "2026",
                "venue": "Journal of Graphs",
                "doi": "10.1234/graph.1",
                "source": "arxiv:2601.01234"
            })
        );
        let second = entity(&archive, "Graph Systems");
        assert_eq!(second["description"], "Second paper.");
        assert_eq!(
            second["properties"],
            json!({
                "authors": "Ada Lovelace and Grace Hopper",
                "year": "2025",
                "venue": "Graph Conference",
                "doi": "10.1234/graph.2",
                "source": "url:https://example.org/second"
            })
        );
        assert_eq!(archive["edges"], json!([]));
    }
}

#[test]
fn forward_and_backward_crossrefs_resolve_exact_paper_endpoints() {
    let child = "@inproceedings{child, title={Child Paper}, crossref={parent}}\n";
    let parent = "@proceedings{parent, year=2026}\n";
    for source_text in [format!("{child}{parent}"), format!("{parent}{child}")] {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("papers.bib");
        let db = tmp.path().join("target.db");
        std::fs::write(&source, source_text).unwrap();
        assert_summary(&import(&tmp, &source, &db).output().unwrap(), 2, 1, 2, 0);
        let archive = export(&tmp, &db);
        assert_papers(&archive, 2);
        let edges = archive["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["source"], entity(&archive, "Child Paper")["id"]);
        assert_eq!(edges[0]["target"], entity(&archive, "parent")["id"]);
        assert_eq!(edges[0]["relation"], "depends_on");
        assert_eq!(edges[0]["weight"], 0.7);
        assert!(uuid::Uuid::parse_str(edges[0]["edge_id"].as_str().unwrap()).is_ok());
    }
}

#[test]
fn balanced_malformed_middle_entry_warns_and_keeps_both_neighbors() {
    for verbose in [false, true] {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("papers.bib");
        let db = tmp.path().join("target.db");
        std::fs::write(
            &source,
            "@article{before, title={Before}}\n\
             @article{bad, title {broken}}\n\
             @article{after, title={After}}\n",
        )
        .unwrap();
        let mut cmd = import(&tmp, &source, &db);
        if verbose {
            cmd.arg("--verbose");
        }
        let output = cmd.output().unwrap();
        assert_summary(&output, 2, 0, 3, 1);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(stderr.contains("warning:"), verbose, "{stderr}");
        if verbose {
            assert!(stderr.contains("line 2"), "located warning: {stderr}");
        }
        let archive = export(&tmp, &db);
        assert_papers(&archive, 2);
        entity(&archive, "Before");
        entity(&archive, "After");
        assert_eq!(archive["edges"], json!([]));
    }
}

#[test]
fn unfinished_tail_warns_once_without_importing_its_embedded_entry() {
    let tmp = TempDir::new().unwrap();
    let source = tmp.path().join("papers.bib");
    let db = tmp.path().join("target.db");
    std::fs::write(
        &source,
        "@article{before, title={Before}}\n\
         @article{unfinished, title={Open field\n\
         @article{embedded, title={Must Not Be Imported}}\n",
    )
    .unwrap();
    let output = import(&tmp, &source, &db)
        .arg("--verbose")
        .output()
        .unwrap();
    assert_summary(&output, 1, 0, 2, 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("warning:").count(), 1, "{stderr}");
    assert!(stderr.contains("line 2"), "{stderr}");
    assert!(stderr.contains("unfinished entry"), "{stderr}");
    let archive = export(&tmp, &db);
    assert_papers(&archive, 1);
    assert_eq!(archive["entities"][0]["name"], "Before");
    assert_eq!(archive["edges"], json!([]));
}

fn archive_contents(mut archive: Value) -> Value {
    // Export time is generated per invocation; every persisted field stays in the comparison.
    archive.as_object_mut().unwrap().remove("exported_at");
    archive
}

fn assert_fatal_before_target_open(label: &str, input: Option<&[u8]>, refusal: &str) {
    for existing in [false, true] {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("refused.bib");
        let db = tmp.path().join("target.db");
        let before_archive = existing.then(|| {
            let seed = tmp.path().join("seed.bib");
            std::fs::write(
                &seed,
                "@article{kept, title={Kept}, crossref={anchor}}\n\
                 @article{anchor, title={Anchor}}\n",
            )
            .unwrap();
            assert_summary(&import(&tmp, &seed, &db).output().unwrap(), 2, 1, 2, 0);
            archive_contents(export(&tmp, &db))
        });
        let before_bytes = existing.then(|| std::fs::read(&db).unwrap());
        if let Some(input) = input {
            std::fs::write(&source, input).unwrap();
        }
        let output = import(&tmp, &source, &db).output().unwrap();
        assert!(
            !output.status.success(),
            "{label} must fail: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "{label} emitted a success summary"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(refusal), "{label}: {stderr}");
        if let Some(before) = before_bytes {
            assert_eq!(
                std::fs::read(&db).unwrap(),
                before,
                "{label} changed DB bytes"
            );
            assert_eq!(
                archive_contents(export(&tmp, &db)),
                before_archive.unwrap(),
                "{label} changed persisted graph content"
            );
        } else {
            for suffix in ["", "-wal", "-shm", "-journal"] {
                assert!(
                    !tmp.path().join(format!("target.db{suffix}")).exists(),
                    "{label} created the target or its SQLite sidecars"
                );
            }
        }
    }
}

#[test]
fn duplicate_and_unresolved_crossrefs_are_fatal_before_target_open() {
    for (label, input, refusal) in [
        (
            "duplicate citation key",
            "@article{duplicate, title={First}}\n@article{duplicate, title={Second}}\n",
            "duplicate BibTeX citation key",
        ),
        (
            "unresolved crossref",
            "@article{valid, title={Would Write}}\n@article{child, crossref={missing}}\n",
            "does not identify an imported entry",
        ),
        (
            "crossref to skipped malformed entry",
            "@article{child, crossref={bad}}\n@article{bad, title {broken}}\n",
            "does not identify an imported entry",
        ),
    ] {
        assert_fatal_before_target_open(label, Some(input.as_bytes()), refusal);
    }
}

#[test]
fn invalid_utf8_and_source_io_errors_are_fatal_before_target_open() {
    let mut invalid = b"@article{valid, title={Would Write}}\n@article{bad, title={".to_vec();
    invalid.push(0xff);
    invalid.extend_from_slice(b"}}\n");
    assert_fatal_before_target_open(
        "invalid UTF-8 after a valid entry",
        Some(&invalid),
        "not UTF-8",
    );
    assert_fatal_before_target_open("missing input file", None, "refused.bib");
}

#[test]
fn oversized_entry_is_fatal_before_target_open() {
    let mut input = b"@article{valid, title={Would Write}}\n@article{large, title={".to_vec();
    input.resize(input.len() + 16 * 1024 * 1024, b'x');
    input.extend_from_slice(b"}}\n");
    assert_fatal_before_target_open(
        "entry larger than 16 MiB",
        Some(&input),
        "raw entry exceeds limit",
    );
}

#[test]
fn explicit_json_overrides_bib_extension_and_json_inference_stays_archive() {
    let tmp = TempDir::new().unwrap();
    let source = tmp.path().join("records.bib");
    let db = tmp.path().join("target.db");
    std::fs::write(&source, r#"[{"kind":"concept","name":"JSON control"}]"#).unwrap();
    let output = import(&tmp, &source, &db)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert_success(&output);
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["entities_imported"], 1);
    assert!(summary.get("adapter").is_none());
    assert_eq!(export(&tmp, &db)["entities"][0]["name"], "JSON control");

    let json_source = tmp.path().join("records.json");
    std::fs::copy(&source, &json_source).unwrap();
    let refused_db = tmp.path().join("refused.db");
    let output = import(&tmp, &json_source, &refused_db).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("parse archive"));
    assert!(!refused_db.exists());
}

#[test]
fn default_kind_refuses_bibtex_before_target_open() {
    for explicit in [false, true] {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("papers.bib");
        let db = tmp.path().join("target.db");
        std::fs::write(&source, "@article{paper, title={Paper}}\n").unwrap();
        let mut cmd = import(&tmp, &source, &db);
        cmd.args(["--default-kind", "concept"]);
        if explicit {
            cmd.args(["--format", "bibtex"]);
        }
        let output = cmd.output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("--default-kind is only supported for CSV/TSV input"));
        assert!(!db.exists());
    }
}
