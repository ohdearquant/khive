use std::path::Path;

use serde::Serialize;
use serde_json::Value;

const READ_KEYS: &[&str] = &[
    "version",
    "profile",
    "site",
    "content_signals",
    "content",
    "tools",
    "skills",
];

pub(crate) struct Manifest {
    pub raw: Value,
    pub origin: String,
    pub digest: String,
    pub ignored_keys: usize,
}

pub(crate) fn read_manifest(source: &Path) -> Result<Manifest, String> {
    let path = source.join(".well-known/arw.json");
    let resolved = path.canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "manifest_missing: .well-known/arw.json is required".to_string()
        } else {
            format!(
                "manifest_malformed: manifest cannot be resolved ({})",
                error.kind()
            )
        }
    })?;
    if !resolved.starts_with(source) {
        return Err("manifest_malformed: manifest resolves outside the source tree".to_string());
    }
    let bytes = std::fs::read(resolved).map_err(|error| {
        format!(
            "manifest_malformed: manifest cannot be read ({})",
            error.kind()
        )
    })?;
    let raw: Value = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "manifest_malformed: invalid JSON at line {} column {}",
            error.line(),
            error.column()
        )
    })?;
    // YAML's mapping decoder rejects duplicate keys; JSON decoding above remains
    // authoritative for JSON syntax and scalar values.
    serde_yaml::from_slice::<serde_yaml::Value>(&bytes)
        .map_err(|_| "manifest_malformed: duplicate or unreadable object keys".to_string())?;
    let object = raw
        .as_object()
        .ok_or_else(|| "manifest_malformed: manifest must be an object".to_string())?;
    let site = object
        .get("site")
        .and_then(Value::as_object)
        .ok_or_else(|| "manifest_malformed: site must be an object".to_string())?;
    if site
        .get("name")
        .and_then(Value::as_str)
        .is_none_or(|name| name.trim().is_empty())
    {
        return Err("manifest_malformed: site.name must be a non-empty string".to_string());
    }
    let homepage = site
        .get("homepage")
        .and_then(Value::as_str)
        .ok_or_else(|| "manifest_malformed: site.homepage must be an absolute URL".to_string())?;
    let url = url::Url::parse(homepage)
        .map_err(|_| "manifest_malformed: site.homepage must be an absolute URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(
            "manifest_malformed: site.homepage must be an HTTP origin without credentials"
                .to_string(),
        );
    }
    let origin = url
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "manifest_malformed: site.homepage must have a host".to_string())?
        .to_lowercase();
    let ignored_keys = object
        .keys()
        .filter(|key| !READ_KEYS.contains(&key.as_str()))
        .count();
    Ok(Manifest {
        raw,
        origin,
        digest: blake3::hash(&bytes).to_hex().to_string(),
        ignored_keys,
    })
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct QuarantinedDeclaration {
    pub field: String,
    pub reason: String,
}

pub(crate) fn cross_check_llms(source: &Path, manifest: &Value) -> Vec<QuarantinedDeclaration> {
    let path = source.join("llms.txt");
    let path = match path.canonicalize() {
        Ok(path) if path.starts_with(source) => path,
        Ok(_) => return quarantine("llms.txt", "file resolves outside the source tree"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => return quarantine("llms.txt", "file cannot be resolved"),
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return quarantine("llms.txt", "file is not readable UTF-8"),
    };
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let opening = line.trim();
        if !matches!(opening, "```yaml" | "```yml") {
            continue;
        }
        let mut yaml = String::new();
        let mut closed = false;
        for line in lines.by_ref() {
            if line.trim() == "```" {
                closed = true;
                break;
            }
            yaml.push_str(line);
            yaml.push('\n');
        }
        if !closed {
            return quarantine("llms.txt", "embedded YAML block is not closed");
        }
        let yaml: serde_yaml::Value = match serde_yaml::from_str(&yaml) {
            Ok(value) => value,
            Err(_) => return quarantine("llms.txt", "embedded YAML is malformed"),
        };
        let summary = match serde_json::to_value(yaml) {
            Ok(Value::Object(mut value)) => {
                value.retain(|key, _| READ_KEYS.contains(&key.as_str()));
                Value::Object(value)
            }
            _ => {
                return quarantine(
                    "llms.txt",
                    "embedded YAML must be an object with string keys",
                )
            }
        };
        let mut mismatches = Vec::new();
        compare_declarations(&summary, Some(manifest), "", &mut mismatches);
        return mismatches;
    }
    Vec::new()
}

fn quarantine(field: &str, reason: &str) -> Vec<QuarantinedDeclaration> {
    vec![QuarantinedDeclaration {
        field: field.to_string(),
        reason: reason.to_string(),
    }]
}

fn compare_declarations(
    summary: &Value,
    manifest: Option<&Value>,
    field: &str,
    mismatches: &mut Vec<QuarantinedDeclaration>,
) {
    match (summary, manifest) {
        (Value::Object(summary), Some(Value::Object(manifest))) => {
            for (key, value) in summary {
                let next = if field.is_empty() {
                    key.clone()
                } else {
                    format!("{field}.{key}")
                };
                compare_declarations(value, manifest.get(key), &next, mismatches);
            }
        }
        (Value::Array(summary), Some(Value::Array(manifest)))
            if matches!(field, "content" | "tools" | "skills") =>
        {
            let key = if field == "content" { "url" } else { "name" };
            for (index, entry) in summary.iter().enumerate() {
                let identity = entry.get(key).and_then(Value::as_str);
                let original = identity.and_then(|identity| {
                    manifest
                        .iter()
                        .find(|candidate| candidate.get(key).and_then(Value::as_str) == Some(identity))
                });
                compare_declarations(entry, original, &format!("{field}[{index}]"), mismatches);
            }
        }
        _ if manifest == Some(summary) => {}
        _ => mismatches.push(QuarantinedDeclaration {
            field: field.to_string(),
            reason: "llms.txt declaration disagrees with the authoritative manifest; manifest value retained".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cross_check_compares_only_declared_fields_and_matches_pages_by_url() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("llms.txt"), "# Meadow Archive\n```yaml\nversion: '1.0'\nmanifest: /.well-known/arw.json\nsite:\n  name: Different name\ncontent:\n  - url: /plants\n    description: Plant index\n```\n").unwrap();
        let manifest = json!({"version":"1.0", "site":{"name":"Meadow Archive", "homepage":"https://meadow.invalid"}, "content":[{"url":"/about"}, {"url":"/plants", "description":"Plant index", "tags":["botany"]}]});
        let results = cross_check_llms(&source.path().canonicalize().unwrap(), &manifest);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].field, "site.name");
        assert!(results[0].reason.contains("manifest value retained"));
    }

    #[test]
    fn duplicate_yaml_keys_are_quarantined() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(
            source.path().join("llms.txt"),
            "```yaml\nsite:\n  name: one\n  name: two\n```\n",
        )
        .unwrap();
        let results = cross_check_llms(&source.path().canonicalize().unwrap(), &json!({}));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].reason, "embedded YAML is malformed");
    }
}
