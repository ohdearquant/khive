use super::*;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn schema_defaults_and_section_serialization_are_strict() {
    assert!(KhiveConfig::default().schema.strict);
    for input in ["", "[schema]\n", "[schema]\nstrict = true\n"] {
        assert!(toml::from_str::<KhiveConfig>(input).unwrap().schema.strict);
    }
    for strict in [true, false] {
        let section = SchemaSectionConfig { strict };
        let encoded = toml::to_string(&section).unwrap();
        assert_eq!(
            toml::from_str::<SchemaSectionConfig>(&encoded)
                .unwrap()
                .strict,
            strict
        );
        let config: KhiveConfig = toml::from_str(&format!("[schema]\n{encoded}")).unwrap();
        assert_eq!(config.schema.strict, strict);
    }
}

#[test]
fn schema_invalid_type_names_selected_file_key_and_actual_location() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("selected.toml");
    for (input, key, line) in [
        (
            "# policy\n[schema]\nstrict = \"false\"\n",
            "schema.strict",
            3,
        ),
        ("[schema]\nstrict = 0\n", "schema.strict", 2),
        ("[schema]\nstrict = []\n", "schema.strict", 2),
        ("schema = false\n", "schema", 1),
        ("schema = []\n", "schema", 1),
    ] {
        std::fs::write(&path, input).unwrap();
        let error = KhiveConfig::load(Some(&path)).unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains(path.canonicalize().unwrap().to_str().unwrap()),
            "{text}"
        );
        assert!(text.contains(key), "{text}");
        assert!(text.contains(&format!("line {line}")), "{text}");
        assert!(text.contains("column"), "{text}");
        let ConfigError::SchemaParse { source, .. } = error else {
            panic!("missing schema diagnostic")
        };
        assert!(source.span().is_some());
    }
}

#[test]
fn schema_unknown_keys_warn_ignore_and_never_disclose_values() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("selected.toml");
    std::fs::write(
        &path,
        "[schema]\nstrict = false\nfuture = \"private-value\"\n",
    )
    .unwrap();
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let config = tracing::subscriber::with_default(subscriber, || {
        KhiveConfig::load(Some(&path)).unwrap().unwrap()
    });
    assert!(!config.schema.strict);
    let text = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("ignoring unknown schema configuration key"),
        "{text}"
    );
    assert!(text.contains("future"), "{text}");
    assert!(!text.contains("private-value"), "{text}");
    // A malformed unrelated policy still refuses the entire selected file.
    std::fs::write(
        &path,
        "[schema]\nstrict = false\nfuture = 1\n[gate]\nunknown = true\n",
    )
    .unwrap();
    assert!(KhiveConfig::load(Some(&path)).is_err());
}

#[test]
fn schema_loader_selects_whole_file_by_existing_precedence() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    let home = tmp.path().join("home");
    let db_dir = tmp.path().join("database");
    for dir in [&project, &home.join(".khive"), &db_dir.join(".khive")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let db = db_dir.join("target.db");
    let tier2 = project.join("khive.toml");
    let tier3 = db_dir.join(".khive/config.toml");
    let tier4 = home.join(".khive/config.toml");
    std::fs::write(&tier4, "[schema]\nstrict = false\n").unwrap();
    assert!(
        !KhiveConfig::load_with_roots(&project, Some(&home), Some(&db))
            .unwrap()
            .unwrap()
            .schema
            .strict
    );
    std::fs::write(&tier3, "[schema]\n").unwrap();
    assert!(
        KhiveConfig::load_with_roots(&project, Some(&home), Some(&db))
            .unwrap()
            .unwrap()
            .schema
            .strict
    );
    std::fs::write(&tier2, "[schema]\nstrict = false\n").unwrap();
    assert!(
        !KhiveConfig::load_with_roots(&project, Some(&home), Some(&db))
            .unwrap()
            .unwrap()
            .schema
            .strict
    );
    std::fs::write(&tier2, "[schema]\nstrict = \"false\"\n").unwrap();
    assert!(KhiveConfig::load_with_roots(&project, Some(&home), Some(&db)).is_err());
    std::fs::remove_file(tier2).unwrap();
    std::fs::remove_file(tier3).unwrap();
    std::fs::remove_file(tier4).unwrap();
    std::fs::create_dir_all(project.join(".khive")).unwrap();
    std::fs::write(
        project.join(".khive/khive.toml"),
        "[schema]\nstrict = false\n",
    )
    .unwrap();
    assert!(
        KhiveConfig::load_with_roots(&project, Some(&home), Some(&db))
            .unwrap()
            .is_none()
    );
    let missing = project.join("explicit-missing.toml");
    assert!(matches!(
        KhiveConfig::load_with_home_fallback_and_source(Some(&missing), Some(&db)),
        Err(ConfigError::ExplicitConfigMissing { .. })
    ));
}
