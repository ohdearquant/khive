use std::io::Write;
use std::sync::{Arc, Mutex};

use super::*;
use crate::engine_config::KhiveConfig;

#[test]
fn file_load_preserves_omission_and_positive_batch_sizes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("selected.toml");
    assert_eq!(KhiveConfig::default().embed.batch_size, None);
    for (raw, expected) in [
        ("", None),
        ("[embed]\n", None),
        ("[embed]\nbatch_size = 1\n", Some(1)),
        ("[embed]\nbatch_size = 64\n", Some(64)),
        ("[embed]\nbatch_size = 500\n", Some(500)),
        ("[embed]\nbatch_size = 5000000000\n", Some(5_000_000_000)),
    ] {
        std::fs::write(&path, raw).unwrap();
        let config = KhiveConfig::load(Some(&path)).unwrap().unwrap();
        assert_eq!(config.embed.batch_size, expected, "{raw}");
        assert_eq!(
            toml::from_str::<KhiveConfig>(raw).unwrap().embed.batch_size,
            expected,
            "direct deserialize: {raw}"
        );
    }
}

#[test]
fn invalid_batch_values_name_selected_file_key_and_actual_source_location() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("selected.toml");
    let invalid = ["0", "-1", "1.5", "\"64\"", "[]", "true"];
    for value in invalid {
        let raw = format!("# selected file\n\n[embed]\nbatch_size = {value}\n");
        assert_invalid(&path, &raw, "embed.batch_size", 4);
    }
    for value in [
        "false",
        "[]",
        "[64]",
        "\"table\"",
        "64",
        "1979-05-27T07:32:00Z",
        "1979-05-27",
    ] {
        let raw = format!("# selected file\n\nembed = {value}\n");
        assert_invalid(&path, &raw, "embed", 3);
    }
}

fn assert_invalid(path: &Path, raw: &str, key: &str, line: usize) {
    std::fs::write(path, raw).unwrap();
    let error = KhiveConfig::load(Some(path)).unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains(path.canonicalize().unwrap().to_str().unwrap()),
        "{text}"
    );
    assert!(text.contains(key), "{text}");
    assert!(text.contains(&format!("line {line}")), "{text}");
    assert!(text.contains("column"), "{text}");
    let ConfigError::SchemaParse {
        key: actual_key,
        source,
        ..
    } = error
    else {
        panic!("expected span-preserving policy error: {text}")
    };
    assert_eq!(actual_key, key);
    let span = source.span().expect("original TOML error span");
    assert!(
        span.start < span.end && span.end <= raw.len(),
        "{span:?}: {text}"
    );
    let actual_line = raw[..span.start]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1;
    assert_eq!(
        actual_line, line,
        "span must identify the original failing value: {text}"
    );
    assert!(toml::from_str::<KhiveConfig>(raw).is_err(), "{raw}");
}

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
fn unknown_keys_warn_without_values_and_preserve_known_settings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("selected.toml");
    std::fs::write(
        &path,
        "[embed]\nbatch_size = 64\nauto_embed = false\nmodel = 'pending-model'\ndimensions = 384\ndevice = 'cpu'\nfuture = 'private-outer-value'\n[embed.fields]\ninclude = ['name']\nfuture_field = 'private-inner-value'\n",
    ).unwrap();
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
    assert_eq!(config.embed.batch_size, Some(64));
    assert!(!config.embed.auto_embed);
    assert_eq!(config.embed.fields.include(), &["name"]);
    assert_eq!(config.embed.model.as_deref(), Some("pending-model"));
    assert_eq!(config.embed.dimensions, Some(384));
    assert_eq!(config.embed.device, Some(LegacyEmbedDevice::Cpu));
    let text = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        text.matches("ignoring unknown embed configuration key")
            .count(),
        2,
        "{text}"
    );
    assert!(text.contains("future"), "{text}");
    assert!(text.contains("fields.future_field"), "{text}");
    for absent in [
        "legacy [embed] settings are ignored",
        "private-outer-value",
        "private-inner-value",
        "pending-model",
        "auto_embed",
        "dimensions",
        "device",
        "include",
    ] {
        assert!(!text.contains(absent), "{text}");
    }
    // The open embed table must not loosen an unrelated policy table.
    std::fs::write(&path, "[embed]\nbatch_size = 64\n[gate]\nunknown = true\n").unwrap();
    assert!(KhiveConfig::load(Some(&path)).is_err());
}

#[test]
fn selected_file_omission_does_not_merge_and_invalid_value_never_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(home.join(".khive")).unwrap();
    std::fs::write(
        home.join(".khive/config.toml"),
        "[embed]\nbatch_size = 64\n",
    )
    .unwrap();
    let load = || KhiveConfig::load_with_roots(&project, Some(&home), None);
    assert_eq!(load().unwrap().unwrap().embed.batch_size, Some(64));
    let selected = project.join("khive.toml");
    std::fs::write(&selected, "[embed]\n").unwrap();
    assert_eq!(load().unwrap().unwrap().embed.batch_size, None);
    std::fs::write(&selected, "[embed]\nbatch_size = 500\n").unwrap();
    assert_eq!(load().unwrap().unwrap().embed.batch_size, Some(500));
    std::fs::write(&selected, "[embed]\nbatch_size = 0\n").unwrap();
    let error = load().unwrap_err().to_string();
    assert!(
        error.contains(selected.canonicalize().unwrap().to_str().unwrap()),
        "{error}"
    );
    assert!(error.contains("embed.batch_size"), "{error}");
}

#[test]
fn embed_known_values_keep_source_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("selected.toml");
    for (key, invalid) in [
        ("auto_embed", vec!["0", "'false'", "[]", "{}"]),
        ("model", vec!["''", "false", "384", "[]", "{}"]),
        ("dimensions", vec!["0", "-1", "1.5", "'384'", "false", "[]"]),
        (
            "device",
            vec![
                "''",
                "'CPU'",
                "'other'",
                "0",
                "false",
                "[]",
                "{}",
                "{cpu = {}}",
                "{cpu = []}",
            ],
        ),
        (
            "fields",
            vec![
                "false",
                "[]",
                "[['name']]",
                "'name'",
                "42",
                "1979-05-27T07:32:00Z",
                "1979-05-27",
            ],
        ),
    ] {
        let expected = match key {
            "auto_embed" => "embed.auto_embed",
            "model" => "embed.model",
            "dimensions" => "embed.dimensions",
            "device" => "embed.device",
            "fields" => "embed.fields",
            _ => unreachable!(),
        };
        for value in invalid {
            let raw = format!("# selected file\n\n[embed]\n{key} = {value}\n");
            assert_invalid(&path, &raw, expected, 4);
        }
    }
    for value in [
        "[]",
        "['']",
        "['   ']",
        "['kind']",
        "['name', 'name']",
        "[42]",
        "false",
        "'name'",
        "{}",
    ] {
        let raw = format!("# selected file\n\n[embed.fields]\ninclude = {value}\n");
        assert_invalid(&path, &raw, "embed.fields.include", 4);
    }
    assert_invalid(
        &path,
        "# selected file\n\n[embed.fields]\ninclude = [\n  'name',\n  42,\n]\n",
        "embed.fields.include",
        6,
    );
    for value in ["{}", "[]"] {
        let raw = format!("# selected file\n\n[embed.device]\ncpu = {value}\n");
        assert_invalid(&path, &raw, "embed.device", 3);
    }
    assert_invalid(
        &path,
        "# dotted key\n\nembed.auto_embed = 'false'\n",
        "embed.auto_embed",
        3,
    );
}

#[test]
fn parsed_policy_defaults_and_exact_selectors_do_not_select_providers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("selected.toml");
    for raw in ["", "[embed]\n", "[embed.fields]\n"] {
        std::fs::write(&path, raw).unwrap();
        let config = KhiveConfig::load(Some(&path)).unwrap().unwrap();
        assert_eq!(config.embed, EmbedSectionConfig::default());
        assert!(config.embed.auto_embed);
        assert_eq!(config.embed.fields.include(), &["name", "description"]);
        assert_eq!(
            toml::from_str::<KhiveConfig>(raw).unwrap().embed,
            config.embed
        );
    }
    let raw = "[embed]\nauto_embed = false\nmodel = ' '\ndimensions = 5000000000\ndevice = 'metal'\n[embed.fields]\ninclude = ['a.b', ' name ', 'Kind', 'kind ', 'description', 'name']\n";
    std::fs::write(&path, raw).unwrap();
    let config = KhiveConfig::load(Some(&path)).unwrap().unwrap();
    assert!(!config.embed.auto_embed);
    assert_eq!(config.embed.model.as_deref(), Some(" "));
    assert_eq!(config.embed.dimensions, Some(5_000_000_000));
    assert_eq!(config.embed.device, Some(LegacyEmbedDevice::Metal));
    assert_eq!(
        config.embed.fields.include(),
        &["a.b", " name ", "Kind", "kind ", "description", "name"]
    );
    assert!(
        config.engines.is_empty(),
        "legacy declarations must not create engines"
    );
    assert_eq!(
        toml::from_str::<KhiveConfig>(raw).unwrap().embed,
        config.embed
    );
    for device in ["metal", "cuda", "cpu"] {
        std::fs::write(&path, format!("[embed]\ndevice = '{device}'\n")).unwrap();
        assert!(KhiveConfig::load(Some(&path))
            .unwrap()
            .unwrap()
            .embed
            .device
            .is_some());
    }
}

#[test]
fn invalid_selected_policy_never_falls_back_or_merges() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(home.join(".khive")).unwrap();
    std::fs::write(
        home.join(".khive/config.toml"),
        "[embed]\nauto_embed = false\nmodel = 'fallback'\n[embed.fields]\ninclude = ['fallback']\n",
    )
    .unwrap();
    let selected = project.join("khive.toml");
    std::fs::write(&selected, "[embed]\n").unwrap();
    assert_eq!(
        KhiveConfig::load_with_roots(&project, Some(&home), None)
            .unwrap()
            .unwrap()
            .embed,
        EmbedSectionConfig::default()
    );
    for (key, raw) in [
        ("embed.auto_embed", "[embed]\nauto_embed = 'false'\n"),
        ("embed.model", "[embed]\nmodel = ''\n"),
        ("embed.dimensions", "[embed]\ndimensions = 0\n"),
        ("embed.device", "[embed]\ndevice = 'other'\n"),
        ("embed.fields", "[embed]\nfields = []\n"),
        ("embed.fields.include", "[embed.fields]\ninclude = []\n"),
    ] {
        std::fs::write(&selected, raw).unwrap();
        let error = KhiveConfig::load_with_roots(&project, Some(&home), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains(key), "{error}");
        assert!(
            error.contains(selected.canonicalize().unwrap().to_str().unwrap()),
            "{error}"
        );
    }
}

#[test]
fn literal_datetime_marker_after_ordinary_keys_remains_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("selected.toml");
    for raw in [
        "[embed]\nbatch_size = 64\n'$__toml_private_datetime' = 'private-outer-value'\n[embed.fields]\ninclude = ['name']\n'$__toml_private_datetime' = 'private-inner-value'\n",
        "embed = { batch_size = 64, '$__toml_private_datetime' = 'private-outer-value', fields = { include = ['name'], '$__toml_private_datetime' = 'private-inner-value' } }\n",
    ] {
        std::fs::write(&path, raw).unwrap();
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time().with_ansi(false).with_writer(move || writer.clone()).finish();
        let config = tracing::subscriber::with_default(subscriber, || {
            KhiveConfig::load(Some(&path)).expect("unknown marker keys remain accepted").unwrap()
        });
        assert_eq!(config.embed.batch_size, Some(64));
        assert_eq!(config.embed.fields.include(), &["name"]);
        assert_eq!(toml::from_str::<KhiveConfig>(raw).unwrap().embed, config.embed);
        let text = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.matches("ignoring unknown embed configuration key").count(), 2, "{text}");
        assert!(text.contains("$__toml_private_datetime"), "{text}");
        assert!(text.contains("fields.$__toml_private_datetime"), "{text}");
        assert!(!text.contains("private-outer-value"), "{text}");
        assert!(!text.contains("private-inner-value"), "{text}");
    }
}

/// The `[embed]` section every released `kkernel kg init` wrote (v0.2.1 to v0.11.0), verbatim,
/// still loads: each of its values is valid under the strict rules, so an upgrade refuses none.
#[test]
fn the_config_released_initializers_wrote_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("khive.toml");
    let raw = "[embed]\nmodel = \"mE5-small\"\ndimensions = 384\nauto_embed = true\nbatch_size = 64\n\n[embed.fields]\ninclude = [\"name\", \"description\"]\n";
    std::fs::write(&path, raw).unwrap();
    let config = KhiveConfig::load(Some(&path))
        .unwrap_or_else(|error| panic!("{error}"))
        .unwrap();
    assert_eq!(config.embed.model.as_deref(), Some("mE5-small"));
    assert_eq!(config.embed.dimensions, Some(384));
    assert_eq!(config.embed.device, None);
    assert!(config.embed.auto_embed);
    assert_eq!(config.embed.batch_size, Some(64));
    assert_eq!(config.embed.fields.include(), &["name", "description"]);
}
