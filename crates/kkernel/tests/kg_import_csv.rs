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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
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

#[test]
fn explicit_csv_and_inferred_delimited_formats_import_all_rows() {
    for (extension, separator, explicit) in [
        ("data", ",", true),
        ("csv", ",", false),
        ("tsv", "\t", false),
    ] {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join(format!("records.{extension}"));
        let db = tmp.path().join("target.db");
        std::fs::write(&source, format!("name{separator}description{separator}year\nAlpha{separator}first{separator}2024\nBeta{separator}second{separator}2025\nGamma{separator}third{separator}2026\n")).unwrap();
        let mut cmd = command(&tmp);
        cmd.args(["kg", "import"])
            .arg(&source)
            .arg("--db")
            .arg(&db)
            .args(["--default-kind", "concept"]);
        if explicit {
            cmd.args(["--format", "csv"]);
        }
        assert_success(&cmd.output().unwrap());
        let archive = export(&tmp, &db);
        let entities = archive["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 3);
        for (name, description, year) in [
            ("Alpha", "first", "2024"),
            ("Beta", "second", "2025"),
            ("Gamma", "third", "2026"),
        ] {
            let entity = entities
                .iter()
                .find(|entity| entity["name"] == name)
                .unwrap();
            assert_eq!(entity["kind"], "concept");
            assert_eq!(entity["description"], description);
            assert_eq!(entity["properties"], json!({"year":year}));
            assert!(uuid::Uuid::parse_str(entity["id"].as_str().unwrap()).is_ok());
        }
        assert_eq!(archive["edges"], json!([]));
    }
}

#[test]
fn invalid_later_row_does_not_open_or_mutate_target_database() {
    for existing in [false, true] {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("records.csv");
        let db = tmp.path().join("target.db");
        let mut cmd = command(&tmp);
        cmd.args(["kg", "import"]).arg(&source).arg("--db").arg(&db);
        if existing {
            std::fs::write(&source, "name,kind\nKept,concept\n").unwrap();
            assert_success(&cmd.output().unwrap());
        }
        let before = existing.then(|| std::fs::read(&db).unwrap());
        std::fs::write(
            &source,
            "name,kind\nWouldWrite,concept\nRefused,not_a_kind\n",
        )
        .unwrap();
        let output = cmd.output().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("not_a_kind"), "{stderr}");
        if let Some(before) = before {
            assert_eq!(std::fs::read(&db).unwrap(), before);
            let archive = export(&tmp, &db);
            assert_eq!(archive["entities"].as_array().unwrap().len(), 1);
            assert_eq!(archive["entities"][0]["name"], "Kept");
        } else {
            assert!(!db.exists());
        }
    }
}

#[test]
fn explicit_format_overrides_extension_and_json_is_not_inferred() {
    let tmp = TempDir::new().unwrap();
    let source = tmp.path().join("records.csv");
    let db = tmp.path().join("target.db");
    std::fs::write(&source, r#"[{"kind":"concept","name":"JSON"}]"#).unwrap();
    let result = command(&tmp)
        .args(["kg", "import"])
        .arg(&source)
        .arg("--db")
        .arg(&db)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert_success(&result);
    assert_eq!(export(&tmp, &db)["entities"][0]["name"], "JSON");

    let archive_path = tmp.path().join("archive.ndjson");
    std::fs::copy(tmp.path().join("export.json"), &archive_path).unwrap();
    let archive_db = tmp.path().join("archive.db");
    let result = command(&tmp)
        .args(["kg", "import"])
        .arg(archive_path)
        .arg("--db")
        .arg(&archive_db)
        .output()
        .unwrap();
    assert_success(&result);
    assert_eq!(export(&tmp, &archive_db)["entities"][0]["name"], "JSON");

    let generic_json = tmp.path().join("records.json");
    std::fs::copy(source, &generic_json).unwrap();
    let refused_db = tmp.path().join("refused.db");
    let result = command(&tmp)
        .args(["kg", "import"])
        .arg(&generic_json)
        .arg("--db")
        .arg(&refused_db)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("parse archive"));
    assert!(!refused_db.exists());
}

#[test]
fn standalone_edge_list_retains_existing_same_import_endpoint_refusal() {
    let tmp = TempDir::new().unwrap();
    let source = tmp.path().join("entities.csv");
    let db = tmp.path().join("target.db");
    std::fs::write(&source, "id,name,kind\n11111111-1111-1111-1111-111111111111,A,concept\n22222222-2222-2222-2222-222222222222,B,concept\n").unwrap();
    assert_success(
        &command(&tmp)
            .args(["kg", "import"])
            .arg(&source)
            .arg("--db")
            .arg(&db)
            .output()
            .unwrap(),
    );
    let before = std::fs::read(&db).unwrap();
    let edges = tmp.path().join("edges.csv");
    std::fs::write(&edges, "source,target,relation\n11111111-1111-1111-1111-111111111111,22222222-2222-2222-2222-222222222222,extends\n").unwrap();
    let output = command(&tmp)
        .args(["kg", "import"])
        .arg(edges)
        .arg("--db")
        .arg(&db)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("is not present in adapter entities"));
    assert_eq!(std::fs::read(&db).unwrap(), before);
    let archive = export(&tmp, &db);
    assert_eq!(archive["entities"].as_array().unwrap().len(), 2);
    assert_eq!(archive["edges"], json!([]));
}
