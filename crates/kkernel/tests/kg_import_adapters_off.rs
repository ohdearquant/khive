//! Without the `kg-adapters` feature, `kg import` reads native archives only.
//! Every other format, named or inferred from the file extension, is refused
//! before the source is read, and the destination database is never created.
#![cfg(not(feature = "kg-adapters"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::json;
use tempfile::TempDir;

struct Fixture {
    directory: TempDir,
    db: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let db = directory.path().join("target/import.db");
        Self { directory, db }
    }

    fn import(&self, source: &Path, format: Option<&str>) -> Output {
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
            .current_dir(self.directory.path())
            .env("HOME", self.directory.path())
            .env(
                "KHIVE_VOLUME_LOCK_DIR",
                self.directory.path().join("volume-locks"),
            )
            .env("KHIVE_NO_DAEMON", "1")
            .args(["--log", "off", "kg", "import"])
            .arg(source);
        if let Some(format) = format {
            command.args(["--format", format]);
        }
        command.arg("--db").arg(&self.db).output().unwrap()
    }

    fn write(&self, name: &str, body: &str) -> PathBuf {
        let path = self.directory.path().join(name);
        std::fs::write(&path, body).unwrap();
        path
    }
}

#[test]
fn adapter_formats_are_refused_before_the_destination_exists() {
    let cases: [(&str, Option<&str>, &str); 6] = [
        ("input.json", Some("json"), "json"),
        ("input.ndjson", Some("ndjson"), "ndjson"),
        ("input.txt", Some("csv"), "csv"),
        ("input.csv", None, "csv"),
        ("input.tsv", None, "tsv"),
        ("input.bib", None, "bibtex"),
    ];
    for (file, format, name) in cases {
        let fixture = Fixture::new();
        let source = fixture.write(file, "not read\n");
        let output = fixture.import(&source, format);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{file} {format:?} was accepted");
        assert!(
            stderr.contains("kg-adapters") && stderr.contains(&format!("of {name} input")),
            "{file} {format:?} refusal does not name the feature and format: {stderr}"
        );
        assert!(
            !fixture.db.exists() && !fixture.db.parent().unwrap().exists(),
            "{file} {format:?} created the destination"
        );
    }
}

#[test]
fn missing_adapter_sources_are_refused_before_file_read() {
    let cases: [(&str, Option<&str>, &str); 2] = [
        ("missing.txt", Some("csv"), "csv"),
        ("missing.bib", None, "bibtex"),
    ];
    for (file, format, name) in cases {
        let fixture = Fixture::new();
        let source = fixture.directory.path().join(file);
        assert!(
            !source.exists(),
            "missing source fixture unexpectedly exists"
        );
        let output = fixture.import(&source, format);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{file} {format:?} was accepted");
        assert!(
            stderr.contains("kg-adapters") && stderr.contains(&format!("of {name} input")),
            "{file} {format:?} refusal does not name the feature and format: {stderr}"
        );
        assert!(
            !stderr.contains(&format!("read {}", source.display())),
            "{file} {format:?} returned a source-read error: {stderr}"
        );
        assert!(
            output.stdout.is_empty(),
            "{file} {format:?} emitted output for a refused import"
        );
        assert!(
            !fixture.db.exists() && !fixture.db.parent().unwrap().exists(),
            "{file} {format:?} created the destination"
        );
    }
}

#[test]
fn a_native_archive_still_imports() {
    let fixture = Fixture::new();
    let archive = json!({
        "format": "khive-kg", "version": "0.1", "namespace": "local",
        "exported_at": "2026-01-02T03:04:05Z", "entities": [], "edges": []
    });
    let source = fixture.write("archive.json", &archive.to_string());
    let output = fixture.import(&source, None);
    assert!(
        output.status.success(),
        "native import failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fixture.db.exists(),
        "native import did not create its destination"
    );
}
