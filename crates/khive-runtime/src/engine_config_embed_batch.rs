//! The implemented `[embed].batch_size` slice of ADR-035.

use std::path::Path;

use serde::Deserialize;

use super::ConfigError;

/// Explicit maintenance batch configuration from `[embed]`.
///
/// Omission leaves the caller's default in effect. A present value is positive;
/// the reindex caller applies its effective maximum of 500 after CLI precedence.
/// Other accepted `[embed]` settings are not implemented by this section yet.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct EmbedSectionConfig {
    #[serde(default, deserialize_with = "positive_batch_size")]
    pub batch_size: Option<u64>,
}

fn positive_batch_size<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "embed.batch_size must be a positive integer",
        ));
    }
    Ok(Some(value))
}

// Struct deserialization alone can accept a sequence. Require a table for both
// the original-text probe and direct KhiveConfig deserialization.
pub(super) fn deserialize_table<'de, D>(deserializer: D) -> Result<EmbedSectionConfig, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct TableVisitor;
    impl<'de> serde::de::Visitor<'de> for TableVisitor {
        type Value = EmbedSectionConfig;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an embed configuration table")
        }

        fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            EmbedSectionConfig::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }
    deserializer.deserialize_map(TableVisitor)
}

#[derive(Deserialize)]
struct SyntaxProbe {
    #[serde(default, deserialize_with = "deserialize_table")]
    embed: EmbedSectionConfig,
}

pub(super) fn parse(
    raw: &str,
    document: &toml::Value,
    path: &Path,
) -> Result<(EmbedSectionConfig, Vec<String>), ConfigError> {
    // Deserialize original bytes so invalid values retain their source span.
    let probe: SyntaxProbe = toml::from_str(raw).map_err(|source| ConfigError::SchemaParse {
        path: path.to_path_buf(),
        key: if document.get("embed").is_some_and(|value| !value.is_table()) {
            "embed"
        } else {
            "embed.batch_size"
        },
        source: Box::new(source),
    })?;
    let mut unknown = Vec::new();
    if let Some(table) = document.get("embed").and_then(toml::Value::as_table) {
        for key in table.keys() {
            // These other accepted keys belong to later slices, not typos.
            if !matches!(
                key.as_str(),
                "batch_size" | "auto_embed" | "fields" | "model" | "dimensions" | "device"
            ) {
                unknown.push(key.clone());
            }
        }
        if let Some(fields) = table.get("fields").and_then(toml::Value::as_table) {
            unknown.extend(
                fields
                    .keys()
                    .filter(|key| key.as_str() != "include")
                    .map(|key| format!("fields.{key}")),
            );
        }
    }
    Ok((probe.embed, unknown))
}

#[cfg(test)]
mod tests {
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
        for value in ["false", "[]", "[64]", "\"table\"", "64"] {
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
        assert!(source.span().is_some(), "{text}");
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
    fn unknown_keys_warn_without_values_and_pending_keys_remain_recognized() {
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
}
